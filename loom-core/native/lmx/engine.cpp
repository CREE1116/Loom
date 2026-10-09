// Loom's native LMX v2 engine. Runs a native LMX file with ggml's kernels (Metal, CUDA,
// Vulkan or CPU), one sequence at a time.
//
// Memory model: resident weights live in one backend buffer. Routed experts never load
// as a whole: each layer's router runs first, its chosen experts are paged from the file
// into a bounded slot pool (LRU), and the expert FFN then runs on the slots. The token
// embedding stays in the file; only the rows of the current tokens are read.
//
// The caller (Rust, `lmx_engine.rs`) parses and validates the LMX index and hands over
// absolute file offsets; this file only reads those ranges.

#include <ggml-alloc.h>
#include <ggml-backend.h>
#include <ggml-cpu.h>
#include <ggml.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <exception>
#include <functional>
#include <future>
#include <memory>
#include <mutex>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <string>
#include <thread>
#include <tuple>
#include <unordered_map>
#include <vector>

#ifdef _WIN32
#define NOMINMAX
#include <windows.h>
#else
#include <fcntl.h>
#include <sys/mman.h>
#include <unistd.h>
#endif
#ifdef __APPLE__
#include <mach/mach.h>
#include <sys/sysctl.h>
#endif

extern "C" {

enum LoomLmxKind : int32_t {
    LOOM_EMBEDDING = 0,
    LOOM_OUTPUT,
    LOOM_OUTPUT_NORM,
    LOOM_ATTN_NORM,
    LOOM_POST_NORM,
    LOOM_Q,
    LOOM_K,
    LOOM_V,
    LOOM_O,
    LOOM_Q_NORM,
    LOOM_K_NORM,
    LOOM_QKV,
    LOOM_Z,
    LOOM_BETA,
    LOOM_ALPHA,
    LOOM_CONV,
    LOOM_DT_BIAS,
    LOOM_A_LOG,
    LOOM_SSM_NORM,
    LOOM_SSM_OUT,
    LOOM_ROUTER,
    LOOM_SHARED_GATE,
    LOOM_SHARED_UP,
    LOOM_SHARED_DOWN,
    LOOM_SHARED_ROUTER,
    LOOM_EXPERT_GATE,
    LOOM_EXPERT_UP,
    LOOM_EXPERT_DOWN,
    LOOM_SC_IN,       // short convolution (LFM2): in_proj [3*hidden, hidden]
    LOOM_SC_CONV,     // depthwise kernel [hidden, taps]
    LOOM_SC_OUT,      // out_proj
    LOOM_EXPERT_BIAS, // selection bias of a sigmoid router [experts]
    LOOM_FFN_GATE,    // dense FFN
    LOOM_FFN_UP,
    LOOM_FFN_DOWN,
    LOOM_KIND_COUNT,
};

// Value maps from the stored (HF) form to the executed form.
enum LoomLmxMap : int32_t {
    LOOM_MAP_NONE = 0,
    LOOM_MAP_PLUS_ONE = 1, // zero-centered RMS norm: x * (1 + w)
    LOOM_MAP_NEG_EXP = 2,  // A = -exp(A_log)
};

struct LoomLmxTensor {
    int32_t kind;
    int32_t layer;  // -1 for model-level tensors
    int32_t expert; // -1 unless an expert projection
    int32_t type;   // ggml_type
    int32_t map;
    int32_t pad;
    int64_t ne[4]; // ggml order: ne[0] is the last logical axis
    uint64_t offset;
    uint64_t bytes;
};

struct LoomLmxConfig {
    uint32_t layers, hidden, vocab;
    uint32_t heads, kv_heads, head_dim;
    uint32_t rope_dims;
    int32_t rope_sections[4];
    float rope_theta, rms_eps;
    uint32_t experts, active, expert_ff, shared_ff;
    uint32_t conv_kernel, key_heads, value_heads, key_dim, value_dim;
    uint32_t context;     // KV positions to allocate
    uint32_t chunk;       // max tokens per forward pass
    uint32_t threads;     // CPU threads, 0 = auto
    uint32_t gpu;         // 1 = use a GPU device when present
    uint32_t train_context;
    uint64_t expert_budget; // bytes for the expert slot pools
    uint32_t prefetch;      // experts to predict and page in ahead per layer (decode), 0 = off
    int32_t kv_type;        // ggml type of the KV cache (f16, q8_0, q4_0, ...)
    uint32_t kv_rotate;     // 1 = rotate K/V/Q per head before caching (TurboQuant-style)
    uint32_t pad;
    const uint8_t * mixer;  // [layers], see Mixer
    const uint8_t * dense;  // [layers], 1 = dense FFN, 0 = routed experts
    uint32_t gating;        // 0 = softmax router, 1 = sigmoid router (+ selection bias)
    uint32_t normalize_topk;
    uint32_t rope_neox;     // 0 = interleaved multi-section RoPE, 1 = split-half RoPE
    uint32_t pad2;
};

struct LoomLmxStats {
    uint64_t hits, misses, bytes_read;
    uint64_t resident_bytes, pool_bytes, state_bytes;
    uint32_t slots, pools, position, chunk;
    char backend[64];
    double build_us, alloc_us, compute_us, route_us;
    uint64_t prefetched, prefetch_hits;
    uint64_t budget, all_experts;
    uint64_t warm_total, warm_done;
    uint64_t released;
    uint32_t context, pad2;
    uint64_t spec_tokens, spec_restarts;
};

} // extern "C"

namespace {

struct File {
#ifdef _WIN32
    HANDLE h = INVALID_HANDLE_VALUE;
    explicit File(const char * path) {
        int n = MultiByteToWideChar(CP_UTF8, 0, path, -1, nullptr, 0);
        std::wstring w(n, L'\0');
        MultiByteToWideChar(CP_UTF8, 0, path, -1, w.data(), n);
        h = CreateFileW(w.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING,
                        FILE_ATTRIBUTE_NORMAL, nullptr);
        if (h == INVALID_HANDLE_VALUE) throw std::runtime_error(std::string("open ") + path);
    }
    ~File() {
        if (h != INVALID_HANDLE_VALUE) CloseHandle(h);
    }
    void read(void * dst, size_t n, uint64_t off) const {
        auto * p = static_cast<uint8_t *>(dst);
        while (n > 0) {
            OVERLAPPED o{};
            o.Offset = DWORD(off & 0xffffffffu);
            o.OffsetHigh = DWORD(off >> 32);
            DWORD want = DWORD(std::min<size_t>(n, 1u << 30)), got = 0;
            if (!ReadFile(h, p, want, &got, &o) || got == 0) throw std::runtime_error("read model file");
            p += got, n -= got, off += got;
        }
    }
#else
    int fd = -1;
    explicit File(const char * path) {
        fd = ::open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) throw std::runtime_error(std::string("open ") + path);
#ifdef __APPLE__
        // The engine keeps what it reads in its own slots; a second copy in the page cache
        // would only double the memory a large model takes.
        fcntl(fd, F_NOCACHE, 1);
#endif
    }
    ~File() {
        if (fd >= 0) ::close(fd);
    }
    void read(void * dst, size_t n, uint64_t off) const {
        auto * p = static_cast<uint8_t *>(dst);
        [[maybe_unused]] const uint64_t start = off;
        while (n > 0) {
            ssize_t got = ::pread(fd, p, std::min<size_t>(n, 1u << 30), off_t(off));
            if (got <= 0) throw std::runtime_error("read model file");
            p += got, n -= size_t(got), off += uint64_t(got);
        }
#if defined(__linux__)
        posix_fadvise(fd, off_t(start), off_t(off - start), POSIX_FADV_DONTNEED); // see above
#endif
    }
#endif
    File(const File &) = delete;
    File & operator=(const File &) = delete;
};

// Runs n independent jobs on persistent threads plus the caller. Expert pages come from
// different file ranges, so parallel reads keep an SSD's queue full.
class Workers {
public:
    explicit Workers(unsigned count) {
        for (unsigned i = 0; i < count; ++i) threads_.emplace_back([this] { loop(); });
    }
    ~Workers() {
        {
            std::lock_guard<std::mutex> lock(m_);
            stop_ = true;
        }
        wake_.notify_all();
        for (auto & t : threads_) t.join();
    }
    void run(size_t n, const std::function<void(size_t)> & job) {
        if (n == 0) return;
        {
            std::lock_guard<std::mutex> lock(m_);
            job_ = &job, next_ = 0, count_ = n, finished_ = 0, error_ = nullptr;
        }
        wake_.notify_all();
        work();
        std::unique_lock<std::mutex> lock(m_);
        done_.wait(lock, [&] { return finished_ == count_; });
        job_ = nullptr;
        if (error_) std::rethrow_exception(error_);
    }

private:
    void work() {
        for (;;) {
            size_t i;
            const std::function<void(size_t)> * job;
            {
                std::lock_guard<std::mutex> lock(m_);
                if (!job_ || next_ >= count_) return;
                i = next_++, job = job_;
            }
            std::exception_ptr err;
            try {
                (*job)(i);
            } catch (...) {
                err = std::current_exception();
            }
            std::lock_guard<std::mutex> lock(m_);
            if (err && !error_) error_ = err;
            if (++finished_ == count_) done_.notify_all();
        }
    }
    void loop() {
        for (;;) {
            {
                std::unique_lock<std::mutex> lock(m_);
                wake_.wait(lock, [&] { return stop_ || (job_ && next_ < count_); });
                if (stop_) return;
            }
            work();
        }
    }
    std::vector<std::thread> threads_;
    std::mutex m_;
    std::condition_variable wake_, done_;
    const std::function<void(size_t)> * job_ = nullptr;
    size_t next_ = 0, count_ = 0, finished_ = 0;
    std::exception_ptr error_;
    bool stop_ = false;
};

// Token mixer of a layer.
enum Mixer : uint8_t {
    MIX_GATED_ATTENTION = 0, // Qwen3.5: query carries an output gate
    MIX_DELTANET = 1,        // Qwen3.5 Gated DeltaNet
    MIX_ATTENTION = 2,       // plain GQA (LFM2)
    MIX_SHORTCONV = 3,       // LFM2 gated short convolution
};

constexpr int64_t kNoOwner = -1;
constexpr size_t kNodes = 16384; // graph capacity
constexpr uint64_t kMaxSlots = 1024; // experts one mul_mat_id may index

// One slot pool per expert layout (types and shapes of gate/up/down). A layer's experts all
// share a layout; layers with the same layout share a pool.
struct Pool {
    std::array<int32_t, 3> types{};
    std::array<int64_t, 2> gate_ne{}, down_ne{};
    std::array<ggml_tensor *, 3> w{};
    uint32_t slots = 0, layers = 0;
    std::vector<int64_t> owner;
    std::vector<uint64_t> used;
    std::vector<uint8_t> ahead; // paged in on a prediction, not yet used
    std::vector<int64_t> warm;  // index into the warm-up plan while being filled, else -1
    std::vector<uint8_t> off;   // memory handed back to the OS under pressure
    uint32_t enabled = 0;
    std::unordered_map<int64_t, uint32_t> where;
};

struct ExpertSource {
    std::array<uint64_t, 3> offset{};
    std::array<uint64_t, 3> bytes{};
};

struct Layer {
    std::array<ggml_tensor *, LOOM_KIND_COUNT> t{};
    uint8_t mixer = MIX_GATED_ATTENTION;
    bool linear = false;    // Gated DeltaNet
    bool recurrent = false; // keeps state between tokens (DeltaNet or short convolution)
    bool attention = false; // keeps KV
    bool dense = false;     // dense FFN instead of routed experts
    int pool = -1;
    // Recurrent state, double-buffered: a pass reads [cur] and writes [cur ^ 1]; the write
    // is committed (cur flips) only once the pass is known to be valid.
    ggml_tensor * conv[2] = {}; // [kernel-1, channels]; short convolution: [kernel-1, hidden]
    ggml_tensor * ssm[2] = {};  // [v_dim, v_dim, v_heads]
    uint8_t cur = 0;
    ggml_tensor * k_cache = nullptr, * v_cache = nullptr;
};

bool parity(uint64_t x) {
    bool odd = false;
    for (; x; x &= x - 1) odd = !odd;
    return odd;
}

// Physical memory and what the OS could hand out now without swapping (free plus
// reclaimable cache).
void system_memory(uint64_t & total, uint64_t & available) {
    total = available = 0;
    if (const char * fake = std::getenv("LOOM_LMX_FAKE_AVAILABLE_MB")) { // tests only
        total = 64ull << 30;
        available = std::strtoull(fake, nullptr, 10) << 20;
        return;
    }
#if defined(_WIN32)
    MEMORYSTATUSEX m{};
    m.dwLength = sizeof m;
    if (GlobalMemoryStatusEx(&m)) total = m.ullTotalPhys, available = m.ullAvailPhys;
#elif defined(__APPLE__)
    size_t len = sizeof total;
    sysctlbyname("hw.memsize", &total, &len, nullptr, 0);
    vm_statistics64_data_t vm{};
    mach_msg_type_number_t count = HOST_VM_INFO64_COUNT;
    vm_size_t page = 0;
    host_page_size(mach_host_self(), &page);
    if (host_statistics64(mach_host_self(), HOST_VM_INFO64, reinterpret_cast<host_info64_t>(&vm), &count) == KERN_SUCCESS)
        available = (uint64_t(vm.free_count) + vm.inactive_count + vm.speculative_count + vm.purgeable_count) * page;
#else
    if (FILE * f = std::fopen("/proc/meminfo", "r")) {
        char key[64];
        unsigned long long kb;
        while (std::fscanf(f, "%63s %llu kB", key, &kb) == 2) {
            if (!std::strcmp(key, "MemTotal:")) total = kb * 1024;
            if (!std::strcmp(key, "MemAvailable:")) available = kb * 1024;
        }
        std::fclose(f);
    }
#endif
}

uint32_t pad_to(uint32_t x, uint32_t m) { return (x + m - 1) / m * m; }

struct Engine {
    LoomLmxConfig cfg{};
    std::vector<uint8_t> mixer, dense;
    File file;

    ggml_backend_t gpu = nullptr, cpu = nullptr;
    ggml_backend_dev_t gpu_dev = nullptr;
    ggml_context * slot_ctx = nullptr;
    bool slots_host = false; // slot tensors' data pointers are CPU-writable
    std::vector<std::pair<void *, ggml_backend_buffer_t>> slot_buffers; // (owned host memory, buffer)
    uint64_t budget = 0;       // expert slot bytes chosen
    uint64_t released = 0;     // of which handed back to the OS under memory pressure
    uint64_t expert_bytes = 0; // every routed expert
    ggml_backend_sched_t sched = nullptr;
    std::string backend_name = "CPU";

    ggml_context * wctx = nullptr, * sctx = nullptr, * pctx = nullptr;
    ggml_backend_buffer_t wbuf = nullptr, sbuf = nullptr, pbuf = nullptr;

    std::vector<Layer> layers;
    ggml_tensor * output = nullptr, * output_norm = nullptr;

    int32_t embd_type = -1;
    uint64_t embd_offset = 0;
    size_t embd_row = 0;

    std::vector<ExpertSource> experts; // [layer * E + expert]
    std::vector<Pool> pools;

    ggml_tensor * rot = nullptr, * rot_t = nullptr; // per-head orthogonal rotation and inverse
    ggml_tensor * act_x = nullptr;   // residual stream after attention [hidden, chunk]
    ggml_tensor * act_in = nullptr;  // FFN input (post-attention norm) [hidden, chunk]
    ggml_tensor * act_w = nullptr;   // routed expert weights [1, active, chunk]
    ggml_tensor * act_ids = nullptr; // routed expert slots [active, chunk]
    ggml_tensor * spec_map = nullptr; // expert -> slot per layer [E, L] (0 when absent)
    ggml_tensor * spec_x1 = nullptr, * spec_in = nullptr, * spec_w = nullptr, * spec_ids = nullptr; // per layer
    uint64_t spec_tokens = 0, spec_restarts = 0;

    uint32_t position = 0, context = 0, chunk = 0;
    uint64_t tick = 0;
    ggml_tensor * kv_rows = nullptr; // while building a reusable graph: KV rows to write

    // Whole-model decode graphs kept between tokens, one per recurrent-state parity (the
    // double buffers swap every token). Each has its own scheduler so its allocation
    // survives the layer-by-layer passes; rebuilt when the attended KV length grows.
    struct Cached {
        std::vector<uint8_t> meta;
        ggml_context * ctx = nullptr;
        ggml_cgraph * g = nullptr;
        ggml_backend_sched_t sched = nullptr;
        ggml_tensor * embd = nullptr, * pos = nullptr, * mask = nullptr, * rows = nullptr, * out = nullptr;
        int64_t n_kv = -1;
        void release() {
            if (ctx) ggml_free(ctx);
            if (sched) ggml_backend_sched_free(sched);
            ctx = nullptr, sched = nullptr, n_kv = -1;
        }
    };
    Cached cached[2];
    uint64_t hits = 0, misses = 0, bytes_read = 0;

    std::vector<uint8_t> meta, staging;
    struct Load {
        ggml_tensor * tensor;
        size_t at;
        uint64_t offset;
        size_t bytes;
    };
    std::vector<Load> loads, ahead_loads;
    Workers io{7};
    std::future<void> pending;
    std::vector<int32_t> pred;
    uint64_t prefetched = 0, prefetch_hits = 0;

    // Warm-up: right after load, background threads fill the slot pools in file order
    // (fast sequential reads) so the first request does not page experts in one by one.
    struct WarmItem {
        uint32_t pool, slot;
        int64_t key;
    };
    std::vector<WarmItem> warm_plan;
    // 0 pending, 1 loaded, 2 failed, 3 being read by a request, 4 being read by warm-up
    std::unique_ptr<std::atomic<uint8_t>[]> warm_ready;
    struct Claim {
        Pool * pool;
        uint32_t slot;
        size_t item;
    };
    std::vector<Claim> claims; // warm-up items a request reads itself, ahead of the queue
    std::atomic<size_t> warm_next{0}, warm_done{0};
    std::atomic<uint64_t> warm_bytes{0};
    std::atomic<bool> warm_stop{false};
    std::vector<std::thread> warmers;
    double prof[4] = {}; // build, allocate, compute, route (microseconds)

    Engine(const char * path, const LoomLmxConfig & c) : cfg(c), file(path) {}

    ~Engine() {
        stop_warmup();
        for (auto & cg : cached) cg.release();
        try {
            finish_prefetch();
        } catch (...) {
        }
        if (sched) ggml_backend_sched_free(sched);
        for (auto & [host, b] : slot_buffers) {
            ggml_backend_buffer_free(b);
            if (host) aligned_free_portable(host);
        }
        if (slot_ctx) ggml_free(slot_ctx);
        for (auto * b : {wbuf, sbuf, pbuf})
            if (b) ggml_backend_buffer_free(b);
        for (auto * c : {wctx, sctx, pctx})
            if (c) ggml_free(c);
        if (gpu) ggml_backend_free(gpu);
        if (cpu) ggml_backend_free(cpu);
    }

    ggml_backend_t main_backend() const { return gpu ? gpu : cpu; }

    void init_backends() {
        cpu = ggml_backend_init_by_type(GGML_BACKEND_DEVICE_TYPE_CPU, nullptr);
        if (!cpu) throw std::runtime_error("no CPU backend");
        uint32_t threads = cfg.threads ? cfg.threads : std::max(1u, std::thread::hardware_concurrency() / 2);
        ggml_backend_cpu_set_n_threads(cpu, int(threads));
        if (cfg.gpu) {
            ggml_backend_dev_t dev = ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_GPU);
            if (!dev) dev = ggml_backend_dev_by_type(GGML_BACKEND_DEVICE_TYPE_IGPU);
            if (dev) {
                gpu_dev = dev;
                gpu = ggml_backend_dev_init(dev, nullptr);
                if (gpu) backend_name = ggml_backend_dev_name(dev);
            }
        }
        sched = new_sched();
    }

    ggml_backend_sched_t new_sched() {
        std::vector<ggml_backend_t> list;
        if (gpu) list.push_back(gpu);
        list.push_back(cpu);
        ggml_backend_sched_t s = ggml_backend_sched_new(list.data(), nullptr, int(list.size()), kNodes, false, true);
        if (!s) throw std::runtime_error("ggml scheduler");
        return s;
    }

    // Reads `bytes` at `offset` into `t` at byte `at`, directly when the buffer is host memory.
    void upload(ggml_tensor * t, size_t at, uint64_t offset, size_t bytes) {
        if (ggml_backend_buffer_is_host(t->buffer)) {
            file.read(static_cast<uint8_t *>(t->data) + at, bytes, offset);
        } else {
            constexpr size_t step = 8u << 20;
            staging.resize(std::min(bytes, step));
            for (size_t done = 0; done < bytes; done += step) {
                size_t n = std::min(step, bytes - done);
                file.read(staging.data(), n, offset + done);
                ggml_backend_tensor_set(t, staging.data(), at + done, n);
            }
        }
        bytes_read += bytes;
    }

    void load(const LoomLmxTensor * ts, size_t count) {
        const uint32_t L = cfg.layers, E = cfg.experts;
        layers.assign(L, Layer{});
        for (uint32_t l = 0; l < L; ++l) {
            auto & lay = layers[l];
            lay.mixer = mixer[l];
            if (lay.mixer > MIX_SHORTCONV) throw std::runtime_error("bad layer mixer");
            lay.linear = lay.mixer == MIX_DELTANET;
            lay.recurrent = lay.linear || lay.mixer == MIX_SHORTCONV;
            lay.attention = !lay.recurrent;
            lay.dense = dense[l] != 0;
            if (!lay.dense && E == 0) throw std::runtime_error("routed layer in a model without experts");
        }
        experts.assign(size_t(L) * E, ExpertSource{});

        // Resident tensors.
        size_t resident = 0;
        for (size_t i = 0; i < count; ++i)
            if (ts[i].expert < 0 && ts[i].kind != LOOM_EMBEDDING) ++resident;
        ggml_init_params ip{ggml_tensor_overhead() * (resident + 8), nullptr, true};
        wctx = ggml_init(ip);
        std::vector<std::pair<ggml_tensor *, const LoomLmxTensor *>> pending;
        std::vector<std::array<const LoomLmxTensor *, 3>> layout(L);
        for (size_t i = 0; i < count; ++i) {
            const auto & d = ts[i];
            if (d.kind < 0 || d.kind >= LOOM_KIND_COUNT) throw std::runtime_error("bad tensor kind");
            if (d.layer >= int32_t(L)) throw std::runtime_error("bad tensor layer");
            if (ggml_nbytes_pad_check(d)) throw std::runtime_error("tensor size does not match its type");
            if (d.kind == LOOM_EMBEDDING) {
                embd_type = d.type;
                embd_offset = d.offset;
                embd_row = ggml_row_size(ggml_type(d.type), d.ne[0]);
                if (d.ne[0] != cfg.hidden || !ggml_get_type_traits(ggml_type(d.type))->to_float)
                    throw std::runtime_error("unsupported embedding");
                continue;
            }
            if (d.expert >= 0) {
                int p = d.kind - LOOM_EXPERT_GATE;
                if (p < 0 || p > 2 || d.layer < 0 || uint32_t(d.expert) >= E)
                    throw std::runtime_error("bad expert tensor");
                auto & src = experts[size_t(d.layer) * E + d.expert];
                src.offset[p] = d.offset;
                src.bytes[p] = d.bytes;
                auto & first = layout[d.layer][p];
                if (!first) first = &d;
                else if (first->type != d.type || first->ne[0] != d.ne[0] || first->ne[1] != d.ne[1])
                    throw std::runtime_error("experts of one layer differ in layout");
                continue;
            }
            ggml_tensor * t = ggml_new_tensor(wctx, ggml_type(d.type), 4, d.ne);
            if (d.layer < 0) {
                if (d.kind == LOOM_OUTPUT) output = t;
                else if (d.kind == LOOM_OUTPUT_NORM) output_norm = t;
                else throw std::runtime_error("model-level tensor of a layer kind");
            } else {
                layers[d.layer].t[d.kind] = t;
            }
            pending.emplace_back(t, &d);
        }
        if (embd_type < 0 || !output || !output_norm) throw std::runtime_error("missing embedding, head or final norm");
        wbuf = ggml_backend_alloc_ctx_tensors(wctx, main_backend());
        if (!wbuf) throw std::runtime_error("allocate resident weights");
        ggml_backend_buffer_set_usage(wbuf, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
        for (auto & [t, d] : pending) {
            upload(t, 0, d->offset, d->bytes);
            if (d->map != LOOM_MAP_NONE) {
                if (t->type != GGML_TYPE_F32) throw std::runtime_error("value map on non-F32 tensor");
                std::vector<float> v(ggml_nelements(t));
                ggml_backend_tensor_get(t, v.data(), 0, v.size() * 4);
                for (float & x : v) x = d->map == LOOM_MAP_PLUS_ONE ? x + 1.0f : -std::exp(x);
                ggml_backend_tensor_set(t, v.data(), 0, v.size() * 4);
            }
        }
        for (uint32_t l = 0; l < L; ++l) check_layer(l);

        // Expert pools, one per layout.
        for (uint32_t l = 0; l < L; ++l) {
            if (layers[l].dense) continue;
            auto & lay = layout[l];
            if (!lay[0] || !lay[1] || !lay[2]) throw std::runtime_error("layer without routed experts");
            for (uint32_t e = 0; e < E; ++e)
                for (int p = 0; p < 3; ++p)
                    if (!experts[size_t(l) * E + e].bytes[p]) throw std::runtime_error("missing expert tensor");
            std::array<int32_t, 3> types{lay[0]->type, lay[1]->type, lay[2]->type};
            std::array<int64_t, 2> gne{lay[0]->ne[0], lay[0]->ne[1]}, dne{lay[2]->ne[0], lay[2]->ne[1]};
            if (lay[1]->ne[0] != gne[0] || lay[1]->ne[1] != gne[1] || dne[0] != gne[1] || dne[1] != gne[0])
                throw std::runtime_error("expert shapes disagree");
            int found = -1;
            for (size_t i = 0; i < pools.size(); ++i)
                if (pools[i].types == types && pools[i].gate_ne == gne) found = int(i);
            if (found < 0) {
                pools.push_back(Pool{});
                found = int(pools.size() - 1);
                pools[found].types = types;
                pools[found].gate_ne = gne;
                pools[found].down_ne = dne;
            }
            pools[found].layers++;
            layers[l].pool = found;
        }
    }

    static bool ggml_nbytes_pad_check(const LoomLmxTensor & d) {
        if (d.type < 0 || d.type >= GGML_TYPE_COUNT) return true;
        auto type = ggml_type(d.type);
        if (ggml_blck_size(type) <= 0 || d.ne[0] % ggml_blck_size(type) != 0) return true;
        uint64_t rows = uint64_t(d.ne[1]) * uint64_t(d.ne[2]) * uint64_t(d.ne[3]);
        return ggml_row_size(type, d.ne[0]) * rows != d.bytes;
    }

    void check_layer(uint32_t l) const {
        const auto & t = layers[l].t;
        auto need = [&](std::initializer_list<int> kinds) {
            for (int k : kinds)
                if (!t[k]) throw std::runtime_error("layer " + std::to_string(l) + " misses tensor kind " + std::to_string(k));
        };
        need({LOOM_ATTN_NORM, LOOM_POST_NORM});
        if (layers[l].dense)
            need({LOOM_FFN_GATE, LOOM_FFN_UP, LOOM_FFN_DOWN});
        else
            need({LOOM_ROUTER});
        // The shared expert is optional, but whole.
        if (t[LOOM_SHARED_GATE] || t[LOOM_SHARED_UP] || t[LOOM_SHARED_DOWN] || t[LOOM_SHARED_ROUTER])
            need({LOOM_SHARED_GATE, LOOM_SHARED_UP, LOOM_SHARED_DOWN, LOOM_SHARED_ROUTER});
        if (cfg.gating == 1 && !layers[l].dense) need({LOOM_EXPERT_BIAS});
        switch (layers[l].mixer) {
        case MIX_DELTANET:
            need({LOOM_QKV, LOOM_Z, LOOM_BETA, LOOM_ALPHA, LOOM_CONV, LOOM_DT_BIAS, LOOM_A_LOG, LOOM_SSM_NORM,
                  LOOM_SSM_OUT});
            break;
        case MIX_SHORTCONV:
            need({LOOM_SC_IN, LOOM_SC_CONV, LOOM_SC_OUT});
            if (t[LOOM_SC_CONV]->ne[0] != cfg.conv_kernel || t[LOOM_SC_IN]->ne[1] != 3 * int64_t(cfg.hidden))
                throw std::runtime_error("short convolution shapes disagree");
            break;
        default:
            need({LOOM_Q, LOOM_K, LOOM_V, LOOM_O, LOOM_Q_NORM, LOOM_K_NORM});
        }
    }

    void allocate_state() {
        const uint32_t L = cfg.layers;
        context = pad_to(std::max(cfg.context ? cfg.context : auto_context(), 256u), 256);
        const int64_t channels = int64_t(cfg.key_heads) * cfg.key_dim * 2 + int64_t(cfg.value_heads) * cfg.value_dim;
        const uint32_t E = cfg.experts, K = cfg.active;

        // Pools: split the budget by layer count, at least one layer's worth of distinct
        // experts for a prefill chunk, never more than every expert of the pool's layers.
        chunk = std::max(1u, cfg.chunk);
        uint64_t total_layers = 0;
        for (auto & p : pools) total_layers += p.layers;
        budget = cfg.expert_budget ? cfg.expert_budget : auto_budget();
        expert_bytes = 0;
        for (auto & p : pools) expert_bytes += slot_bytes(p) * cfg.experts * p.layers;
        // Backends bound the experts one mul_mat_id may index (Metal: threads per
        // threadgroup), so a layout whose share exceeds a bank splits into banks, each
        // serving a subset of its layers.
        std::vector<Pool> banks;
        std::vector<std::vector<uint32_t>> layout_layers(pools.size());
        for (uint32_t l = 0; l < L; ++l)
            if (layers[l].pool >= 0) layout_layers[layers[l].pool].push_back(l);
        for (size_t pi = 0; pi < pools.size(); ++pi) {
            const Pool & p = pools[pi];
            uint64_t share = budget * p.layers / std::max<uint64_t>(total_layers, 1);
            uint64_t slots = std::min<uint64_t>(share / slot_bytes(p), uint64_t(E) * p.layers);
            const auto & members = layout_layers[pi];
            uint64_t n_banks = std::min<uint64_t>(std::max<uint64_t>(1, (slots + kMaxSlots - 1) / kMaxSlots), members.size());
            for (uint64_t b = 0; b < n_banks; ++b) {
                Pool q;
                q.types = p.types;
                q.gate_ne = p.gate_ne;
                q.down_ne = p.down_ne;
                for (size_t i = b; i < members.size(); i += n_banks) {
                    layers[members[i]].pool = int(banks.size());
                    q.layers++;
                }
                // The layout's slots, in proportion to the layers this bank serves.
                uint64_t n = (slots * q.layers + members.size() - 1) / members.size();
                n = std::max<uint64_t>(n, std::min<uint64_t>(E, uint64_t(K) * chunk));
                n = std::min<uint64_t>({n, uint64_t(E) * q.layers, kMaxSlots});
                n = std::max<uint64_t>(n, K);
                q.slots = uint32_t(n);
                q.owner.assign(q.slots, kNoOwner);
                q.used.assign(q.slots, 0);
                q.ahead.assign(q.slots, 0);
                q.warm.assign(q.slots, -1);
                q.off.assign(q.slots, 0);
                q.enabled = q.slots;
                // A chunk may not route to more distinct experts than a bank holds; a
                // layer never routes to more than E.
                if (q.slots < E) chunk = std::min<uint32_t>(chunk, std::max<uint32_t>(1, q.slots / K));
                banks.push_back(std::move(q));
            }
        }
        pools = std::move(banks);
        chunk = std::max(1u, chunk);

        ggml_init_params pp{ggml_tensor_overhead() * 4, nullptr, true};
        pctx = ggml_init(pp);
        const int64_t hd = cfg.head_dim;
        const bool rotate = cfg.kv_rotate && hd > 0 && (hd & (hd - 1)) == 0;
        if (rotate) {
            rot = ggml_new_tensor_2d(pctx, GGML_TYPE_F32, hd, hd);
            rot_t = ggml_new_tensor_2d(pctx, GGML_TYPE_F32, hd, hd);
        }
        if (rotate) {
            pbuf = ggml_backend_alloc_ctx_tensors(pctx, main_backend());
            if (!pbuf) throw std::runtime_error("allocate rotation");
            set_rotation(hd);
        }
        allocate_pools();

        ggml_init_params sp{ggml_tensor_overhead() * (size_t(L) * 6 + 16), nullptr, true};
        sctx = ggml_init(sp);
        for (uint32_t l = 0; l < L; ++l) {
            auto & lay = layers[l];
            if (lay.linear) {
                for (int b = 0; b < 2; ++b) {
                    lay.conv[b] = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.conv_kernel - 1, channels);
                    lay.ssm[b] = ggml_new_tensor_3d(sctx, GGML_TYPE_F32, cfg.value_dim, cfg.value_dim, cfg.value_heads);
                }
            } else if (lay.mixer == MIX_SHORTCONV) {
                for (int b = 0; b < 2; ++b)
                    lay.conv[b] = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.conv_kernel - 1, cfg.hidden);
            } else {
                lay.k_cache = ggml_new_tensor_3d(sctx, ggml_type(cfg.kv_type), cfg.head_dim, cfg.kv_heads, context);
                lay.v_cache = ggml_new_tensor_3d(sctx, ggml_type(cfg.kv_type), cfg.head_dim, cfg.kv_heads, context);
            }
        }
        act_x = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.hidden, chunk);
        act_in = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.hidden, chunk);
        // Dense models route nothing; the routing tensors keep one element per row.
        const int64_t Kr = std::max<int64_t>(K, 1), Er = std::max<int64_t>(E, 1);
        act_w = ggml_new_tensor_3d(sctx, GGML_TYPE_F32, 1, Kr, chunk);
        act_ids = ggml_new_tensor_2d(sctx, GGML_TYPE_I32, Kr, chunk);
        // Speculative decoding of one token through every layer (see forward_whole).
        spec_map = ggml_new_tensor_2d(sctx, GGML_TYPE_I32, Er, L);
        spec_x1 = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.hidden, L);
        spec_in = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, cfg.hidden, L);
        spec_w = ggml_new_tensor_2d(sctx, GGML_TYPE_F32, Kr, L);
        spec_ids = ggml_new_tensor_2d(sctx, GGML_TYPE_I32, Kr, L);
        sbuf = ggml_backend_alloc_ctx_tensors(sctx, main_backend());
        if (!sbuf) throw std::runtime_error("allocate state");
        reset();
        start_warmup();
    }

    void start_warmup() {
        const uint32_t E = cfg.experts;
        if (pools.empty() || !slots_host) return;
        std::vector<std::vector<uint32_t>> members(pools.size());
        for (uint32_t l = 0; l < cfg.layers; ++l)
            if (layers[l].pool >= 0) members[layers[l].pool].push_back(l);
        for (uint32_t pi = 0; pi < pools.size(); ++pi) {
            auto & p = pools[pi];
            // Even coverage: expert-major over the bank's layers until the slots run out.
            uint32_t s = 0;
            for (uint32_t e = 0; e < E && s < p.slots; ++e)
                for (uint32_t l : members[pi]) {
                    if (s == p.slots) break;
                    int64_t key = int64_t(l) * E + e;
                    p.owner[s] = key;
                    p.where[key] = s;
                    warm_plan.push_back({pi, s, key});
                    ++s;
                }
        }
        // Read in file order.
        std::sort(warm_plan.begin(), warm_plan.end(), [&](const WarmItem & a, const WarmItem & b) {
            return experts[size_t(a.key)].offset[0] < experts[size_t(b.key)].offset[0];
        });
        for (size_t i = 0; i < warm_plan.size(); ++i) pools[warm_plan[i].pool].warm[warm_plan[i].slot] = int64_t(i);
        warm_ready.reset(new std::atomic<uint8_t>[warm_plan.size()]);
        for (size_t i = 0; i < warm_plan.size(); ++i) warm_ready[i].store(0);
        for (int t = 0; t < 4; ++t)
            warmers.emplace_back([this] {
                constexpr size_t run = 16; // consecutive experts per claim, for sequential IO
                for (;;) {
                    size_t begin = warm_next.fetch_add(run);
                    if (begin >= warm_plan.size()) return;
                    // Fill only while memory is free; the rest fills on demand.
                    if (!warm_stop.load() && short_of_memory()) warm_stop = true;
                    for (size_t i = begin; i < std::min(begin + run, warm_plan.size()); ++i) {
                        uint8_t pending = 0;
                        if (!warm_ready[i].compare_exchange_strong(pending, 4)) { // a request took it
                            ++warm_done;
                            continue;
                        }
                        uint8_t state = 2;
                        if (!warm_stop.load()) {
                            try {
                                const auto & it = warm_plan[i];
                                const auto & src = experts[size_t(it.key)];
                                auto & p = pools[it.pool];
                                for (int k = 0; k < 3; ++k) {
                                    ggml_tensor * w = p.w[k];
                                    if (src.bytes[k] != w->nb[2]) throw std::runtime_error("expert size");
                                    file.read(static_cast<uint8_t *>(w->data) + size_t(it.slot) * w->nb[2], src.bytes[k],
                                              src.offset[k]);
                                    warm_bytes += src.bytes[k];
                                }
                                state = 1;
                            } catch (...) {
                            }
                        }
                        warm_ready[i].store(state);
                        ++warm_done;
                    }
                }
            });
    }

    void stop_warmup() {
        warm_stop = true;
        for (auto & t : warmers) t.join();
        warmers.clear();
    }

    // A slot still being filled by the warm-up must not be evicted or read yet.
    bool warming(const Pool & p, uint32_t s) const {
        if (p.warm[s] < 0) return false;
        uint8_t st = warm_ready[size_t(p.warm[s])].load();
        return st != 1 && st != 2;
    }

    // Waits until a warming slot is filled. False when its read failed or was stopped; the
    // slot is then free and the expert is read the normal way.
    bool wait_warm(Pool & p, uint32_t s) {
        if (p.warm[s] < 0) return true;
        const size_t item = size_t(p.warm[s]);
        // Still queued: read it now with this step's other loads instead of waiting.
        uint8_t pending = 0;
        if (warm_ready[item].compare_exchange_strong(pending, 3)) {
            const auto & src = experts[size_t(p.owner[s])];
            for (int k = 0; k < 3; ++k) loads.push_back({p.w[k], size_t(s) * p.w[k]->nb[2], src.offset[k], src.bytes[k]});
            claims.push_back({&p, s, item});
            return true;
        }
        // Claimed earlier in this step (an expert routed twice): it loads with the others.
        for (auto & c : claims)
            if (c.item == item) return true;
        uint8_t st;
        while ((st = warm_ready[item].load()) == 4 || st == 3) std::this_thread::sleep_for(std::chrono::microseconds(100));
        bool ok = warm_ready[size_t(p.warm[s])].load() == 1;
        p.warm[s] = -1;
        if (!ok) {
            p.where.erase(p.owner[s]);
            p.owner[s] = kNoOwner;
            p.used[s] = 0;
        }
        return ok;
    }

    bool all_warm() const { return warm_done.load() >= warm_plan.size(); }

    // Free memory below the pressure threshold (see adapt_memory).
    static bool short_of_memory() {
        uint64_t total = 0, available = 0;
        system_memory(total, available);
        if (!total || !available) return false;
        const uint64_t low = std::max<uint64_t>(1ull << 30, total / 20);
        return available < low + (1ull << 30);
    }

    // Victim for a new expert: least recently used slot that is not pinned by this step
    // and not still warming. UINT32_MAX when none.
    uint32_t victim(const Pool & p) const {
        uint32_t best = UINT32_MAX;
        uint64_t oldest = UINT64_MAX;
        for (uint32_t s = 0; s < p.slots; ++s)
            if (p.used[s] < oldest && p.used[s] < tick && !p.off[s] && !warming(p, s)) oldest = p.used[s], best = s;
        return best;
    }

    // M = W·D/sqrt(n): Walsh-Hadamard W with fixed random signs D. Orthogonal, so
    // (Mq)·(Mk) = q·k; it spreads outlier channels evenly before K/V are quantized.
    // Stored so that ggml_mul_mat(rot, x) = M x and ggml_mul_mat(rot_t, x) = Mᵀ x.
    void set_rotation(int64_t n) {
        std::vector<float> sign(n);
        uint64_t state = 0x9e3779b97f4a7c15ull;
        for (auto & v : sign) {
            state ^= state << 13, state ^= state >> 7, state ^= state << 17;
            v = (state >> 63) ? -1.0f : 1.0f;
        }
        const float scale = 1.0f / std::sqrt(float(n));
        std::vector<float> m(size_t(n * n)), mt(size_t(n * n));
        for (int64_t i = 0; i < n; ++i)
            for (int64_t k = 0; k < n; ++k) {
                float w = parity(uint64_t(i & k)) ? -scale : scale;
                m[size_t(i * n + k)] = w * sign[k];  // M[i][k], row i contiguous
                mt[size_t(k * n + i)] = w * sign[k]; // Mᵀ[k][i]
            }
        ggml_backend_tensor_set(rot, m.data(), 0, m.size() * sizeof(float));
        ggml_backend_tensor_set(rot_t, mt.data(), 0, mt.size() * sizeof(float));
    }

    ggml_tensor * rotate(ggml_context * c, ggml_tensor * m, ggml_tensor * x) {
        if (!m) return x;
        ggml_tensor * y = ggml_mul_mat(c, m, ggml_reshape_2d(c, x, x->ne[0], ggml_nelements(x) / x->ne[0]));
        return ggml_reshape_3d(c, y, x->ne[0], x->ne[1], x->ne[2]);
    }

    // Slot pools live in host memory the GPU maps when the device can (Apple unified
    // memory, integrated GPUs): the CPU then reads experts from the file straight into the
    // slots, from several threads, with no staging copy. Otherwise they are device buffers.
    void allocate_pools() {
        bool mapped = false;
        if (gpu_dev) {
            ggml_backend_dev_props props{};
            ggml_backend_dev_get_props(gpu_dev, &props);
            mapped = props.caps.buffer_from_host_ptr && std::getenv("LOOM_LMX_DEVICE_SLOTS") == nullptr;
        }
        ggml_init_params ip{ggml_tensor_overhead() * (pools.size() * 3 + 1), nullptr, true};
        slot_ctx = ggml_init(ip);
        for (auto & p : pools) {
            p.w[0] = ggml_new_tensor_3d(slot_ctx, ggml_type(p.types[0]), p.gate_ne[0], p.gate_ne[1], p.slots);
            p.w[1] = ggml_new_tensor_3d(slot_ctx, ggml_type(p.types[1]), p.gate_ne[0], p.gate_ne[1], p.slots);
            p.w[2] = ggml_new_tensor_3d(slot_ctx, ggml_type(p.types[2]), p.down_ne[0], p.down_ne[1], p.slots);
        }
        if (!mapped) {
            ggml_backend_buffer_t b = ggml_backend_alloc_ctx_tensors(slot_ctx, main_backend());
            if (!b) throw std::runtime_error("allocate expert slots");
            ggml_backend_buffer_set_usage(b, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
            slot_buffers.push_back({nullptr, b});
            slots_host = ggml_backend_buffer_is_host(b);
            return;
        }
        slots_host = true; // our memory, mapped for the GPU (Metal reports it as not host)
        constexpr size_t align = 1 << 16;
        for (auto & p : pools) {
            size_t size = 0, largest = 0;
            for (ggml_tensor * t : p.w) size += (ggml_nbytes(t) + align - 1) / align * align, largest = std::max(largest, ggml_nbytes(t));
            void * host = aligned_alloc_portable(align, size);
            if (!host) throw std::runtime_error("allocate expert slots");
            ggml_backend_buffer_t b = ggml_backend_dev_buffer_from_host_ptr(gpu_dev, host, size, largest);
            if (!b) {
                aligned_free_portable(host);
                throw std::runtime_error("map expert slots for the GPU");
            }
            slot_buffers.push_back({host, b});
            ggml_backend_buffer_set_usage(b, GGML_BACKEND_BUFFER_USAGE_WEIGHTS);
            ggml_tallocr talloc = ggml_tallocr_new(b);
            for (ggml_tensor * t : p.w)
                if (ggml_tallocr_alloc(&talloc, t) != GGML_STATUS_SUCCESS) throw std::runtime_error("place expert slots");
        }
    }

    static void * aligned_alloc_portable(size_t align, size_t size) {
#ifdef _WIN32
        return _aligned_malloc(size, align);
#else
        void * p = nullptr;
        return posix_memalign(&p, align, size) == 0 ? p : nullptr;
#endif
    }

    static void aligned_free_portable(void * p) {
#ifdef _WIN32
        _aligned_free(p);
#else
        std::free(p);
#endif
    }

    // KV positions when the caller sets none: the KV cache may take 1/32 of RAM (256 MiB to
    // 4 GiB), as many positions as that holds, a power of two from 8K up to the model's.
    uint32_t auto_context() const {
        uint64_t total = 0, available = 0;
        system_memory(total, available);
        uint64_t full = 0;
        for (auto & lay : layers) full += lay.attention ? 1 : 0;
        uint64_t per_position = 2 * full * ggml_row_size(ggml_type(cfg.kv_type), int64_t(cfg.head_dim) * cfg.kv_heads);
        uint64_t kv_budget = std::clamp<uint64_t>(total / 32, 256ull << 20, 4ull << 30);
        uint64_t positions = per_position ? kv_budget / per_position : cfg.train_context;
        uint32_t ctx = 8192;
        while (uint64_t(ctx) * 2 <= positions && ctx * 2 <= cfg.train_context && ctx < (1u << 30)) ctx *= 2;
        return std::min<uint32_t>(ctx, std::max<uint32_t>(cfg.train_context, 256));
    }

    static uint64_t slot_bytes(const Pool & p) {
        return ggml_row_size(ggml_type(p.types[0]), p.gate_ne[0]) * p.gate_ne[1] +
               ggml_row_size(ggml_type(p.types[1]), p.gate_ne[0]) * p.gate_ne[1] +
               ggml_row_size(ggml_type(p.types[2]), p.down_ne[0]) * p.down_ne[1];
    }

    // Expert slot bytes when the caller sets none: every expert if RAM allows, else what fits
    // beside the OS and other programs:
    //   - a quarter of RAM (at least 4 GiB) stays for everything else;
    //   - on a GPU, the device's free working set bounds it.
    // Capacity is not use: the warm-up only fills slots while memory is free, and slots are
    // handed back under pressure (adapt_memory).
    uint64_t auto_budget() const {
        constexpr uint64_t GiB = 1ull << 30;
        uint64_t all = 0;
        for (auto & p : pools) all += slot_bytes(p) * cfg.experts * p.layers;
        const uint64_t hd = cfg.head_dim, kvh = cfg.kv_heads;
        uint64_t full = 0;
        for (auto & lay : layers) full += lay.attention ? 1 : 0;
        uint64_t kv = 2 * full * ggml_row_size(ggml_type(cfg.kv_type), int64_t(hd * kvh)) * context;
        uint64_t workspace = GiB / 2 + uint64_t(std::max(1u, cfg.chunk)) * cfg.hidden * 64;
        uint64_t resident = wbuf ? ggml_backend_buffer_get_size(wbuf) : 0;
        uint64_t total = 0, available = 0;
        system_memory(total, available);
        auto minus = [](uint64_t a, uint64_t b) { return a > b ? a - b : 0; };
        uint64_t out = all;
        if (total) {
            uint64_t reserve = std::max<uint64_t>(4 * GiB, total / 4);
            out = std::min(out, minus(total, reserve + resident + kv + workspace));
        }
        // What is free right now does not bound the budget: slots fill lazily, the warm-up
        // stops while memory is short, and pressure later hands slots back. (`available` is
        // a snapshot that other programs' caches distort.)
        (void) available;
        if (gpu_dev) {
            size_t free = 0, cap = 0;
            ggml_backend_dev_memory(gpu_dev, &free, &cap);
            if (free) out = std::min(out, minus(free, GiB / 2 + kv + workspace));
        }
        return std::max<uint64_t>(out, GiB);
    }

    void reset() {
        finish_prefetch();
        ggml_backend_buffer_clear(sbuf, 0);
        position = 0;
        checkpoints.clear(); // their KV is gone
    }

    // ---- graph pieces -------------------------------------------------------------------

    ggml_tensor * norm(ggml_context * c, ggml_tensor * x, ggml_tensor * w) {
        return ggml_mul(c, ggml_rms_norm(c, x, cfg.rms_eps), w);
    }

    static ggml_tensor * l2_norm(ggml_context * c, ggml_tensor * x, float eps) {
        const float n = float(x->ne[0]);
        return ggml_scale(c, ggml_rms_norm(c, x, eps / n), 1.0f / std::sqrt(n));
    }

    ggml_tensor * cols(ggml_context * c, ggml_tensor * t, int64_t n) {
        return ggml_view_2d(c, t, t->ne[0], n, t->nb[1], 0);
    }

    ggml_tensor * linear_attention(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n) {
        auto & lay = layers[l];
        auto & t = lay.t;
        const int64_t kh = cfg.key_heads, vh = cfg.value_heads, kd = cfg.key_dim, vd = cfg.value_dim;
        const int64_t channels = kh * kd * 2 + vh * vd;

        ggml_tensor * qkv = ggml_mul_mat(c, t[LOOM_QKV], x); // [channels, n]
        ggml_tensor * z = ggml_mul_mat(c, t[LOOM_Z], x);     // [vh*vd, n]
        ggml_tensor * beta = ggml_sigmoid(c, ggml_mul_mat(c, t[LOOM_BETA], x));
        beta = ggml_reshape_4d(c, beta, 1, vh, n, 1);
        ggml_tensor * alpha = ggml_mul_mat(c, t[LOOM_ALPHA], x);
        ggml_tensor * gate = ggml_mul(c, ggml_softplus(c, ggml_add(c, alpha, t[LOOM_DT_BIAS])), t[LOOM_A_LOG]);
        gate = ggml_reshape_4d(c, gate, 1, vh, n, 1);

        // Causal depthwise conv over the last kernel-1 inputs plus this chunk.
        ggml_tensor * conv_state = ggml_reshape_3d(c, lay.conv[lay.cur], cfg.conv_kernel - 1, channels, 1);
        ggml_tensor * conv_next = ggml_reshape_3d(c, lay.conv[lay.cur ^ 1], cfg.conv_kernel - 1, channels, 1);
        ggml_tensor * qkv_t = ggml_transpose(c, ggml_reshape_3d(c, qkv, channels, n, 1));
        ggml_tensor * conv_in = ggml_concat(c, conv_state, qkv_t, 0); // [kernel-1+n, channels, 1]
        ggml_tensor * last = ggml_view_3d(c, conv_in, cfg.conv_kernel - 1, channels, 1, conv_in->nb[1], conv_in->nb[2],
                                          ggml_row_size(conv_in->type, n));
        ggml_build_forward_expand(g, ggml_cpy(c, last, conv_next));
        ggml_tensor * conv = ggml_silu(c, ggml_ssm_conv(c, conv_in, t[LOOM_CONV])); // [channels, n, 1]

        const size_t row = ggml_row_size(conv->type, channels);
        ggml_tensor * q = ggml_view_4d(c, conv, kd, kh, n, 1, ggml_row_size(conv->type, kd), row, row * n, 0);
        ggml_tensor * k = ggml_view_4d(c, conv, kd, kh, n, 1, ggml_row_size(conv->type, kd), row, row * n,
                                       ggml_row_size(conv->type, kh * kd));
        ggml_tensor * v = ggml_view_4d(c, conv, vd, vh, n, 1, ggml_row_size(conv->type, vd), row, row * n,
                                       ggml_row_size(conv->type, 2 * kh * kd));
        q = l2_norm(c, q, cfg.rms_eps);
        k = l2_norm(c, k, cfg.rms_eps);

        ggml_tensor * state = ggml_reshape_4d(c, lay.ssm[lay.cur], vd, vd, vh, 1);
        ggml_tensor * out = ggml_gated_delta_net(c, q, k, v, gate, beta, state, 1);
        ggml_tensor * attn = ggml_view_4d(c, out, vd, vh, n, 1, ggml_row_size(out->type, vd),
                                          ggml_row_size(out->type, vd * vh), ggml_row_size(out->type, vd * vh * n), 0);
        ggml_tensor * new_state = ggml_view_1d(c, out, vd * vd * vh, ggml_row_size(out->type, vd * vh * n));
        ggml_build_forward_expand(g, ggml_cpy(c, new_state, ggml_reshape_1d(c, lay.ssm[lay.cur ^ 1], vd * vd * vh)));

        ggml_tensor * zz = ggml_reshape_4d(c, z, vd, vh, n, 1);
        ggml_tensor * y = ggml_mul(c, norm(c, attn, t[LOOM_SSM_NORM]), ggml_silu(c, zz));
        y = ggml_reshape_2d(c, y, vd * vh, n);
        return ggml_mul_mat(c, t[LOOM_SSM_OUT], y);
    }

    ggml_tensor * full_attention(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n,
                                 ggml_tensor * pos, ggml_tensor * mask, int64_t n_kv) {
        auto & lay = layers[l];
        auto & t = lay.t;
        const int64_t hd = cfg.head_dim, H = cfg.heads, KV = cfg.kv_heads;

        const bool gated = lay.mixer == MIX_GATED_ATTENTION;
        ggml_tensor * q, * gate = nullptr;
        if (gated) {
            ggml_tensor * qg = ggml_mul_mat(c, t[LOOM_Q], x); // [2*hd*H, n]: per head [query, gate]
            const size_t es = ggml_element_size(qg);
            q = ggml_view_3d(c, qg, hd, H, n, es * hd * 2, es * hd * 2 * H, 0);
            gate = ggml_view_3d(c, qg, hd, H, n, es * hd * 2, es * hd * 2 * H, es * hd);
            gate = ggml_cont_2d(c, gate, hd * H, n);
        } else {
            q = ggml_reshape_3d(c, ggml_mul_mat(c, t[LOOM_Q], x), hd, H, n);
        }
        q = norm(c, q, t[LOOM_Q_NORM]);

        ggml_tensor * k = ggml_reshape_3d(c, ggml_mul_mat(c, t[LOOM_K], x), hd, KV, n);
        k = norm(c, k, t[LOOM_K_NORM]);
        ggml_tensor * v = ggml_reshape_3d(c, ggml_mul_mat(c, t[LOOM_V], x), hd, KV, n);

        if (cfg.rope_neox) {
            // Positions are the first n of the multi-section layout.
            ggml_tensor * p1 = ggml_view_1d(c, pos, n, 0);
            q = ggml_rope_ext(c, q, p1, nullptr, int(cfg.rope_dims), GGML_ROPE_TYPE_NEOX, int(cfg.train_context),
                              cfg.rope_theta, 1.0f, 0.0f, 1.0f, 32.0f, 1.0f);
            k = ggml_rope_ext(c, k, p1, nullptr, int(cfg.rope_dims), GGML_ROPE_TYPE_NEOX, int(cfg.train_context),
                              cfg.rope_theta, 1.0f, 0.0f, 1.0f, 32.0f, 1.0f);
        } else {
            int sections[4] = {cfg.rope_sections[0], cfg.rope_sections[1], cfg.rope_sections[2], cfg.rope_sections[3]};
            q = ggml_rope_multi(c, q, pos, nullptr, int(cfg.rope_dims), sections, GGML_ROPE_TYPE_IMROPE,
                                int(cfg.train_context), cfg.rope_theta, 1.0f, 0.0f, 1.0f, 32.0f, 1.0f);
            k = ggml_rope_multi(c, k, pos, nullptr, int(cfg.rope_dims), sections, GGML_ROPE_TYPE_IMROPE,
                                int(cfg.train_context), cfg.rope_theta, 1.0f, 0.0f, 1.0f, 32.0f, 1.0f);
        }

        q = rotate(c, rot, q);
        k = rotate(c, rot, k);
        v = rotate(c, rot, v);
        ggml_tensor * kc = lay.k_cache, * vc = lay.v_cache;
        if (kv_rows) {
            // Rows from an input tensor, so the graph does not depend on the position.
            ggml_build_forward_expand(g, ggml_set_rows(c, ggml_reshape_2d(c, kc, hd * KV, context),
                                                       ggml_reshape_2d(c, k, hd * KV, n), kv_rows));
            ggml_build_forward_expand(g, ggml_set_rows(c, ggml_reshape_2d(c, vc, hd * KV, context),
                                                       ggml_reshape_2d(c, v, hd * KV, n), kv_rows));
        } else {
            ggml_build_forward_expand(
                g, ggml_cpy(c, k, ggml_view_3d(c, kc, hd, KV, n, kc->nb[1], kc->nb[2], kc->nb[2] * position)));
            ggml_build_forward_expand(
                g, ggml_cpy(c, v, ggml_view_3d(c, vc, hd, KV, n, vc->nb[1], vc->nb[2], vc->nb[2] * position)));
        }

        ggml_tensor * kv_k = ggml_permute(c, ggml_view_3d(c, kc, hd, KV, n_kv, kc->nb[1], kc->nb[2], 0), 0, 2, 1, 3);
        ggml_tensor * kv_v = ggml_permute(c, ggml_view_3d(c, vc, hd, KV, n_kv, vc->nb[1], vc->nb[2], 0), 0, 2, 1, 3);
        ggml_tensor * qp = ggml_permute(c, q, 0, 2, 1, 3);
        ggml_tensor * a = ggml_flash_attn_ext(c, qp, kv_k, kv_v, mask, 1.0f / std::sqrt(float(hd)), 0.0f, 0.0f);
        ggml_prec_set_acc(a, GGML_PREC_F32);
        a = rotate(c, rot_t, ggml_reshape_3d(c, a, hd, H, n));
        a = ggml_reshape_2d(c, a, hd * H, n);
        if (gate) a = ggml_mul(c, a, ggml_sigmoid(c, gate));
        return ggml_mul_mat(c, t[LOOM_O], a);
    }

    // LFM2 gated short convolution: in_proj gives [b, c, x]; y = c * conv(b * x) with a
    // causal depthwise kernel over the last taps-1 inputs plus this chunk; then out_proj.
    ggml_tensor * short_conv(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n) {
        auto & lay = layers[l];
        auto & t = lay.t;
        const int64_t D = cfg.hidden, taps = cfg.conv_kernel;
        ggml_tensor * bcx = ggml_mul_mat(c, t[LOOM_SC_IN], x); // [3D, n]
        const size_t es = ggml_element_size(bcx);
        auto part = [&](int i) { return ggml_view_3d(c, bcx, D, n, 1, bcx->nb[1], bcx->nb[1] * n, size_t(i) * D * es); };
        ggml_tensor * bx = ggml_transpose(c, ggml_mul(c, part(0), part(2))); // [n, D, 1]
        ggml_tensor * state = ggml_reshape_3d(c, lay.conv[lay.cur], taps - 1, D, 1);
        ggml_tensor * next = ggml_reshape_3d(c, lay.conv[lay.cur ^ 1], taps - 1, D, 1);
        ggml_tensor * in = ggml_concat(c, state, bx, 0); // [taps-1+n, D, 1]
        ggml_tensor * last = ggml_view_3d(c, in, taps - 1, D, 1, in->nb[1], in->nb[2], ggml_row_size(in->type, n));
        ggml_build_forward_expand(g, ggml_cpy(c, last, next));
        ggml_tensor * conv = ggml_ssm_conv(c, in, t[LOOM_SC_CONV]); // [D, n, 1]
        ggml_tensor * y = ggml_reshape_2d(c, ggml_mul(c, part(1), conv), D, n);
        return ggml_mul_mat(c, t[LOOM_SC_OUT], y);
    }

    ggml_tensor * mix(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n, ggml_tensor * pos,
                      ggml_tensor * mask, int64_t n_kv) {
        switch (layers[l].mixer) {
        case MIX_DELTANET: return linear_attention(c, g, l, x, n);
        case MIX_SHORTCONV: return short_conv(c, g, l, x, n);
        default: return full_attention(c, g, l, x, n, pos, mask, n_kv);
        }
    }

    // A dense layer whole: mixer, then the dense FFN; returns the residual stream after it.
    ggml_tensor * dense_layer(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n,
                              ggml_tensor * pos, ggml_tensor * mask, int64_t n_kv) {
        auto & t = layers[l].t;
        ggml_tensor * x1 = ggml_add(c, mix(c, g, l, norm(c, x, t[LOOM_ATTN_NORM]), n, pos, mask, n_kv), x);
        ggml_tensor * in = norm(c, x1, t[LOOM_POST_NORM]);
        ggml_tensor * gate = ggml_mul_mat(c, t[LOOM_FFN_GATE], in);
        ggml_tensor * up = ggml_mul_mat(c, t[LOOM_FFN_UP], in);
        return ggml_add(c, ggml_mul_mat(c, t[LOOM_FFN_DOWN], ggml_swiglu_split(c, gate, up)), x1);
    }

    // Router scores: what top-k selects on, and what the chosen experts are weighted by.
    std::pair<ggml_tensor *, ggml_tensor *> router(ggml_context * c, uint32_t l, ggml_tensor * in) {
        auto & t = layers[l].t;
        ggml_tensor * logits = ggml_mul_mat(c, t[LOOM_ROUTER], in); // [E, n]
        if (cfg.gating == 1) {
            ggml_tensor * probs = ggml_sigmoid(c, logits);
            ggml_tensor * select = t[LOOM_EXPERT_BIAS] ? ggml_add(c, probs, t[LOOM_EXPERT_BIAS]) : probs;
            return {select, probs};
        }
        ggml_tensor * probs = ggml_soft_max(c, logits);
        return {probs, probs};
    }

    struct Routed {
        ggml_tensor * x1;  // residual after attention [D, n]
        ggml_tensor * in;  // FFN input [D, n]
        ggml_tensor * ids; // routed experts, packed [K, n]
        ggml_tensor * w;   // their weights [1, K, n]
    };

    // Attention of layer l on x, then the router.
    Routed attend_route(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n, ggml_tensor * pos,
                        ggml_tensor * mask, int64_t n_kv) {
        auto & t = layers[l].t;
        ggml_tensor * h = mix(c, g, l, norm(c, x, t[LOOM_ATTN_NORM]), n, pos, mask, n_kv);
        ggml_tensor * x1 = ggml_add(c, h, x);
        ggml_tensor * in = norm(c, x1, t[LOOM_POST_NORM]);

        const int64_t E = cfg.experts, K = cfg.active;
        auto [select, probs] = router(c, l, in);                                     // [E, n]
        ggml_tensor * ids = ggml_argsort_top_k(c, select, int(K));                   // [K, n]
        ggml_tensor * w = ggml_get_rows(c, ggml_reshape_3d(c, probs, 1, E, n), ids); // [1, K, n]
        if (cfg.normalize_topk) {
            w = ggml_reshape_2d(c, w, K, n);
            w = ggml_div(c, w, ggml_clamp(c, ggml_sum_rows(c, w), 6.103515625e-5f, INFINITY));
            w = ggml_reshape_3d(c, w, 1, K, n);
        }
        // argsort_top_k is a strided view; keep a packed copy.
        return {x1, in, ggml_cont(c, ids), w};
    }

    // Attention and router of layer l, leaving the residual, the FFN input and the routing
    // weights in the persistent activations; returns the routed expert ids.
    ggml_tensor * attention_and_router(ggml_context * c, ggml_cgraph * g, uint32_t l, ggml_tensor * x, int64_t n,
                                       ggml_tensor * pos, ggml_tensor * mask, int64_t n_kv,
                                       ggml_tensor ** predicted) {
        const int64_t K = cfg.active;
        auto [x1, in, ids, w] = attend_route(c, g, l, x, n, pos, mask, n_kv);
        ggml_build_forward_expand(g, ggml_cpy(c, x1, cols(c, act_x, n)));
        ggml_build_forward_expand(g, ggml_cpy(c, in, cols(c, act_in, n)));
        ggml_build_forward_expand(g, ggml_cpy(c, w, ggml_view_3d(c, act_w, 1, K, n, act_w->nb[1], act_w->nb[2], 0)));
        // The next layer's router on this layer's FFN input predicts the experts it will
        // pick; the residual stream changes little between adjacent layers.
        if (predicted) {
            ggml_tensor * next = router(c, l + 1, in).first;
            *predicted = ggml_cont(c, ggml_argsort_top_k(c, next, int(std::min<uint32_t>(cfg.prefetch, cfg.experts))));
            ggml_set_output(*predicted);
            ggml_build_forward_expand(g, *predicted);
        }
        ggml_set_output(ids);
        ggml_build_forward_expand(g, ids);
        return ids;
    }

    // Routed experts (on their slots) plus the gated shared expert of layer l; returns the
    // residual stream after the layer.
    ggml_tensor * experts_ffn(ggml_context * c, uint32_t l, int64_t n) {
        const int64_t K = cfg.active;
        return ffn(c, l, n, cols(c, act_in, n), cols(c, act_x, n), ggml_view_3d(c, act_w, 1, K, n, act_w->nb[1], act_w->nb[2], 0),
                   ggml_view_2d(c, act_ids, K, n, act_ids->nb[1], 0));
    }

    // The FFN of layer l: routed experts on their slots plus the gated shared expert, added
    // to the residual x1.
    ggml_tensor * ffn(ggml_context * c, uint32_t l, int64_t n, ggml_tensor * in, ggml_tensor * x1, ggml_tensor * w,
                      ggml_tensor * ids) {
        auto & t = layers[l].t;
        auto & p = pools[layers[l].pool];
        const int64_t K = cfg.active, D = cfg.hidden;

        ggml_tensor * x3 = ggml_reshape_3d(c, in, D, 1, n);
        ggml_tensor * up = ggml_mul_mat_id(c, p.w[1], x3, ids);   // [ff, K, n]
        ggml_tensor * gate = ggml_mul_mat_id(c, p.w[0], x3, ids); // [ff, K, n]
        ggml_tensor * y = ggml_mul_mat_id(c, p.w[2], ggml_swiglu_split(c, gate, up), ids); // [D, K, n]
        y = ggml_mul(c, y, w);
        ggml_tensor * moe = nullptr;
        for (int64_t i = 0; i < K; ++i) {
            ggml_tensor * yi = ggml_view_2d(c, y, D, n, y->nb[2], size_t(i) * y->nb[1]);
            moe = moe ? ggml_add(c, moe, yi) : yi;
        }

        if (!t[LOOM_SHARED_GATE]) return ggml_add(c, moe, x1);
        ggml_tensor * sg = ggml_mul_mat(c, t[LOOM_SHARED_GATE], in);
        ggml_tensor * su = ggml_mul_mat(c, t[LOOM_SHARED_UP], in);
        ggml_tensor * shared = ggml_mul_mat(c, t[LOOM_SHARED_DOWN], ggml_swiglu_split(c, sg, su));
        shared = ggml_mul(c, shared, ggml_sigmoid(c, ggml_mul_mat(c, t[LOOM_SHARED_ROUTER], in)));

        return ggml_add(c, ggml_add(c, moe, shared), x1);
    }

    // ---- execution ----------------------------------------------------------------------

    struct Graph {
        ggml_context * ctx = nullptr;
        ggml_cgraph * g = nullptr;
        ~Graph() {
            if (ctx) ggml_free(ctx);
        }
    };

    void begin(Graph & gr) {
        constexpr size_t nodes = kNodes;
        meta.resize(ggml_tensor_overhead() * nodes + ggml_graph_overhead_custom(nodes, false));
        ggml_init_params ip{meta.size(), meta.data(), true};
        gr.ctx = ggml_init(ip);
        gr.g = ggml_new_graph_custom(gr.ctx, nodes, false);
    }

    // LOOM_LMX_SPLITS=1: report, once per graph shape, the ops the scheduler could not keep
    // on the main backend (each split is a device round trip).
    void report_splits(Graph & gr) {
        static const bool on = std::getenv("LOOM_LMX_SPLITS") != nullptr;
        if (!on) return;
        static std::unordered_map<int, int> seen;
        int nodes = ggml_graph_n_nodes(gr.g);
        if (seen[nodes]++) return;
        std::fprintf(stderr, "lmx graph: %d nodes, %d splits\n", nodes, ggml_backend_sched_get_n_splits(sched));
        for (int i = 0; i < nodes; ++i) {
            ggml_tensor * t = ggml_graph_node(gr.g, i);
            ggml_backend_t b = ggml_backend_sched_get_tensor_backend(sched, t);
            if (b && b != main_backend())
                std::fprintf(stderr, "  on %s: %s %s [%lld,%lld,%lld]\n", ggml_backend_name(b), ggml_op_desc(t), t->name,
                             (long long) t->ne[0], (long long) t->ne[1], (long long) t->ne[2]);
        }
    }

    void compute(Graph & gr) {
        report_splits(gr);
        if (ggml_backend_sched_graph_compute(sched, gr.g) != GGML_STATUS_SUCCESS)
            throw std::runtime_error("graph compute failed");
    }

    // Maps routed expert ids of layer l to pool slots, paging missing experts in.
    void route(uint32_t l, const std::vector<int32_t> & ids, int64_t n) {
        finish_prefetch();
        auto & p = pools[layers[l].pool];
        const uint32_t E = cfg.experts;
        ++tick;
        std::vector<int32_t> slots(ids.size());
        // Pin hits first so misses never evict an expert this chunk uses.
        std::vector<size_t> missing;
        for (size_t i = 0; i < ids.size(); ++i) {
            if (ids[i] < 0 || uint32_t(ids[i]) >= E) throw std::runtime_error("router chose a bad expert");
            int64_t key = int64_t(l) * E + ids[i];
            auto it = p.where.find(key);
            if (it != p.where.end() && wait_warm(p, it->second)) {
                p.used[it->second] = tick;
                if (p.ahead[it->second]) p.ahead[it->second] = 0, ++prefetch_hits;
                slots[i] = int32_t(it->second);
            } else {
                missing.push_back(i);
            }
        }
        for (size_t i : missing) {
            int64_t key = int64_t(l) * E + ids[i];
            auto it = p.where.find(key);
            if (it != p.where.end()) { // repeated within this chunk
                slots[i] = int32_t(it->second);
                ++hits;
                continue;
            }
            uint32_t victim = this->victim(p);
            while (victim == UINT32_MAX && !all_warm()) {
                // Every free slot is still warming: wait for one rather than fail.
                std::this_thread::sleep_for(std::chrono::microseconds(500));
                victim = this->victim(p);
            }
            if (victim == UINT32_MAX) throw std::runtime_error("expert pool smaller than one chunk's experts");
            p.warm[victim] = -1;
            if (p.owner[victim] != kNoOwner) p.where.erase(p.owner[victim]);
            const auto & src = experts[size_t(key)];
            for (int k = 0; k < 3; ++k) {
                ggml_tensor * w = p.w[k];
                if (src.bytes[k] != w->nb[2]) throw std::runtime_error("expert size does not match its pool");
                loads.push_back({w, size_t(victim) * w->nb[2], src.offset[k], src.bytes[k]});
            }
            p.owner[victim] = key;
            p.used[victim] = tick;
            p.ahead[victim] = 0;
            p.where[key] = victim;
            slots[i] = int32_t(victim);
            ++misses;
        }
        try {
            if (!loads.empty() && slots_host) {
                io.run(loads.size(), [&](size_t i) {
                    const Load & d = loads[i];
                    file.read(static_cast<uint8_t *>(d.tensor->data) + d.at, d.bytes, d.offset);
                });
                for (auto & d : loads) bytes_read += d.bytes;
            } else {
                for (auto & d : loads) upload(d.tensor, d.at, d.offset, d.bytes);
            }
        } catch (...) {
            for (auto & c : claims) warm_ready[c.item].store(2);
            claims.clear();
            loads.clear();
            throw;
        }
        for (auto & c : claims) {
            warm_ready[c.item].store(1);
            c.pool->warm[c.slot] = -1;
        }
        claims.clear();
        loads.clear();
        hits += ids.size() - missing.size();
        ggml_backend_tensor_set(act_ids, slots.data(), 0, slots.size() * sizeof(int32_t));
        (void) n;
    }

    // Pages in predicted experts of layer l in the background while the GPU computes. The
    // slots are claimed now (marked used by the next routing step), so routing never
    // evicts them and the running graph never reads them.
    void prefetch(uint32_t l, const std::vector<int32_t> & ids) {
        auto & p = pools[layers[l].pool];
        const uint32_t E = cfg.experts;
        for (int32_t id : ids) {
            if (id < 0 || uint32_t(id) >= E) continue;
            int64_t key = int64_t(l) * E + id;
            if (p.where.count(key)) continue;
            uint32_t victim = this->victim(p);
            if (victim == UINT32_MAX) break; // every slot serves the running or next step
            p.warm[victim] = -1;
            if (p.owner[victim] != kNoOwner) p.where.erase(p.owner[victim]);
            const auto & src = experts[size_t(key)];
            for (int k = 0; k < 3; ++k) {
                ggml_tensor * w = p.w[k];
                if (src.bytes[k] != w->nb[2]) throw std::runtime_error("expert size does not match its pool");
                ahead_loads.push_back({w, size_t(victim) * w->nb[2], src.offset[k], src.bytes[k]});
            }
            p.owner[victim] = key;
            p.used[victim] = tick + 1;
            p.ahead[victim] = 1;
            p.where[key] = victim;
            ++prefetched;
        }
        if (ahead_loads.empty()) return;
        pending = std::async(std::launch::async, [this] {
            if (slots_host) {
                io.run(ahead_loads.size(), [&](size_t i) {
                    const Load & d = ahead_loads[i];
                    file.read(static_cast<uint8_t *>(d.tensor->data) + d.at, d.bytes, d.offset);
                });
            } else {
                for (auto & d : ahead_loads) {
                    std::vector<uint8_t> buf(d.bytes);
                    file.read(buf.data(), d.bytes, d.offset);
                    ggml_backend_tensor_set(d.tensor, buf.data(), d.at, d.bytes);
                }
            }
        });
    }

    void finish_prefetch() {
        if (!pending.valid()) return;
        pending.get();
        for (auto & d : ahead_loads) bytes_read += d.bytes;
        ahead_loads.clear();
    }

    void embed(const int32_t * tokens, int64_t n, std::vector<float> & out) {
        const auto * traits = ggml_get_type_traits(ggml_type(embd_type));
        out.resize(size_t(n) * cfg.hidden);
        std::vector<uint8_t> row(embd_row);
        for (int64_t i = 0; i < n; ++i) {
            if (tokens[i] < 0 || uint32_t(tokens[i]) >= cfg.vocab) throw std::runtime_error("token outside the vocabulary");
            file.read(row.data(), embd_row, embd_offset + uint64_t(tokens[i]) * embd_row);
            traits->to_float(row.data(), out.data() + size_t(i) * cfg.hidden, cfg.hidden);
        }
        bytes_read += uint64_t(n) * embd_row;
    }

    // A slot whose expert can be read by a graph now.
    bool usable(const Pool & p, uint32_t s) const {
        if (p.off[s] || warming(p, s) || p.owner[s] == kNoOwner) return false;
        return p.warm[s] < 0 || warm_ready[size_t(p.warm[s])].load() == 1; // not a skipped warm-up item
    }

    bool resident(uint32_t l, int32_t id) const {
        const auto & p = pools[layers[l].pool];
        auto it = p.where.find(int64_t(l) * cfg.experts + id);
        return it != p.where.end() && usable(p, it->second);
    }

    // Decoding one token layer by layer costs a GPU round trip per layer. When the routed
    // experts are almost always resident, run every layer in one graph instead: the graph
    // looks the experts' slots up itself (spec_map), and the CPU checks afterwards that
    // every routed expert really was resident. At the first layer that routed to a missing
    // expert, the attention part is still valid (it saved its residual, FFN input and
    // weights), so the missing experts are loaded and the pass resumes at that layer's FFN.
    // Recurrent state is double-buffered and committed only for verified layers; KV rows
    // past the verified layer are simply rewritten.
    struct Whole {
        ggml_tensor * embd = nullptr, * pos = nullptr, * mask = nullptr, * rows = nullptr, * out = nullptr;
    };

    // The whole-model graph for one token from layer `start` (after the FFN of start-1
    // when resuming). With `reusable`, KV rows come from an input instead of the position.
    Whole build_whole(ggml_context * c, ggml_cgraph * g, uint32_t start, bool resume, int64_t n_kv, bool reusable) {
        const uint32_t L = cfg.layers, E = cfg.experts, K = cfg.active;
        Whole in;
        ggml_tensor * x;
        if (!resume) {
            in.embd = ggml_new_tensor_2d(c, GGML_TYPE_F32, cfg.hidden, 1);
            ggml_set_input(in.embd);
            x = in.embd;
        } else {
            const uint32_t m = start - 1;
            x = ffn(c, m, 1, ggml_view_2d(c, spec_in, cfg.hidden, 1, spec_in->nb[1], m * spec_in->nb[1]),
                    ggml_view_2d(c, spec_x1, cfg.hidden, 1, spec_x1->nb[1], m * spec_x1->nb[1]),
                    ggml_view_3d(c, spec_w, 1, K, 1, sizeof(float), spec_w->nb[1], m * spec_w->nb[1]),
                    ggml_view_2d(c, act_ids, K, 1, act_ids->nb[1], 0));
        }
        in.pos = ggml_new_tensor_1d(c, GGML_TYPE_I32, 4);
        in.mask = ggml_new_tensor_2d(c, GGML_TYPE_F16, n_kv, 1);
        ggml_set_input(in.pos);
        ggml_set_input(in.mask);
        if (reusable) {
            in.rows = ggml_new_tensor_1d(c, GGML_TYPE_I64, 1);
            ggml_set_input(in.rows);
            kv_rows = in.rows;
        }
        for (uint32_t l = start; l < L; ++l) {
            if (layers[l].dense) {
                x = dense_layer(c, g, l, x, 1, in.pos, in.mask, n_kv);
                continue;
            }
            auto r = attend_route(c, g, l, x, 1, in.pos, in.mask, n_kv);
            auto col = [&](ggml_tensor * t) { return ggml_view_2d(c, t, t->ne[0], 1, t->nb[1], l * t->nb[1]); };
            ggml_build_forward_expand(g, ggml_cpy(c, r.x1, col(spec_x1)));
            ggml_build_forward_expand(g, ggml_cpy(c, r.in, col(spec_in)));
            ggml_build_forward_expand(g, ggml_cpy(c, ggml_reshape_2d(c, r.w, K, 1), col(spec_w)));
            ggml_build_forward_expand(g, ggml_cpy(c, r.ids, col(spec_ids)));
            ggml_tensor * table = ggml_view_2d(c, spec_map, 1, E, sizeof(int32_t), l * spec_map->nb[1]);
            ggml_tensor * slots = ggml_reshape_2d(c, ggml_get_rows(c, table, r.ids), K, 1);
            x = ffn(c, l, 1, r.in, r.x1, r.w, slots);
        }
        kv_rows = nullptr;
        in.out = ggml_mul_mat(c, output, norm(c, x, output_norm));
        ggml_set_output(in.out);
        ggml_build_forward_expand(g, in.out);
        return in;
    }

    void forward_whole(int32_t token, float * logits) {
        const uint32_t L = cfg.layers, E = cfg.experts, K = cfg.active;
        finish_prefetch();
        std::vector<float> embd;
        embed(&token, 1, embd);
        const int64_t n_kv = std::min<int64_t>(pad_to(position + 1, 256), context);
        std::vector<int32_t> pos{int32_t(position), int32_t(position), int32_t(position), 0};
        std::vector<ggml_fp16_t> mask(size_t(n_kv), ggml_fp32_to_fp16(-INFINITY));
        for (int64_t j = 0; j <= int64_t(position); ++j) mask[size_t(j)] = ggml_fp32_to_fp16(0.0f);
        std::vector<int32_t> map(size_t(E) * L), ids(size_t(K) * L);

        uint32_t restarts_now = 0;
        uint32_t start = 0; // first layer whose attention this pass runs
        bool resume = false; // the pass begins with the FFN of layer start - 1 (act_ids set)
        ++spec_tokens;
        for (;;) {
            for (uint32_t l = 0; l < L; ++l) {
                if (layers[l].dense) continue;
                const auto & p = pools[layers[l].pool];
                for (uint32_t e = 0; e < E; ++e) {
                    auto it = p.where.find(int64_t(l) * E + e);
                    map[size_t(l) * E + e] = it != p.where.end() && usable(p, it->second) ? int32_t(it->second) : 0;
                }
            }
            if (!map.empty()) ggml_backend_tensor_set(spec_map, map.data(), 0, map.size() * sizeof(int32_t));

            auto t0 = std::chrono::steady_clock::now();
            auto lap = [&t0] {
                auto now = std::chrono::steady_clock::now();
                double d = std::chrono::duration<double, std::micro>(now - t0).count();
                t0 = now;
                return d;
            };
            Graph gr;
            Whole in;
            Cached * cg = nullptr;
            const uint8_t parity = layers[0].cur;
            bool uniform = true;
            for (auto & lay : layers) uniform = uniform && lay.cur == parity;
            if (!resume && uniform) {
                cg = &cached[parity];
                if (cg->n_kv != n_kv) {
                    if (cg->ctx) ggml_free(cg->ctx);
                    cg->meta.resize(ggml_tensor_overhead() * kNodes + ggml_graph_overhead_custom(kNodes, false));
                    ggml_init_params ip{cg->meta.size(), cg->meta.data(), true};
                    cg->ctx = ggml_init(ip);
                    cg->g = ggml_new_graph_custom(cg->ctx, kNodes, false);
                    Whole w = build_whole(cg->ctx, cg->g, 0, false, n_kv, true);
                    cg->embd = w.embd, cg->pos = w.pos, cg->mask = w.mask, cg->rows = w.rows, cg->out = w.out;
                    if (!cg->sched) cg->sched = new_sched();
                    ggml_backend_sched_reset(cg->sched);
                    if (!ggml_backend_sched_alloc_graph(cg->sched, cg->g)) throw std::runtime_error("allocate graph");
                    cg->n_kv = n_kv;
                }
                in = {cg->embd, cg->pos, cg->mask, cg->rows, cg->out};
                prof[0] += lap();
            } else {
                begin(gr);
                in = build_whole(gr.ctx, gr.g, start, resume, n_kv, false);
                prof[0] += lap();
                ggml_backend_sched_reset(sched);
                if (!ggml_backend_sched_alloc_graph(sched, gr.g)) throw std::runtime_error("allocate graph");
            }
            prof[1] += lap();
            if (in.embd) ggml_backend_tensor_set(in.embd, embd.data(), 0, embd.size() * sizeof(float));
            // Unused when no full-attention layer follows the resume point.
            if (in.pos->buffer) ggml_backend_tensor_set(in.pos, pos.data(), 0, pos.size() * sizeof(int32_t));
            if (in.mask->buffer) ggml_backend_tensor_set(in.mask, mask.data(), 0, mask.size() * sizeof(ggml_fp16_t));
            if (in.rows && in.rows->buffer) {
                const int64_t row = position;
                ggml_backend_tensor_set(in.rows, &row, 0, sizeof row);
            }
            if (cg) {
                if (ggml_backend_sched_graph_compute(cg->sched, cg->g) != GGML_STATUS_SUCCESS)
                    throw std::runtime_error("graph compute failed");
            } else {
                compute(gr);
            }
            prof[2] += lap();
            ggml_tensor * out = in.out;
            if (start < L && E) ggml_backend_tensor_get(spec_ids, ids.data() + size_t(start) * K, size_t(start) * K * sizeof(int32_t),
                                                   size_t(L - start) * K * sizeof(int32_t));

            // Verify layer by layer; commit what is valid.
            uint32_t missed = L;
            for (uint32_t l = start; l < L && missed == L; ++l) {
                layers[l].cur ^= 1; // attention of l ran on valid input
                if (layers[l].dense) continue;
                bool all = true;
                for (uint32_t k = 0; k < K; ++k) all = all && resident(l, ids[size_t(l) * K + k]);
                if (!all) {
                    missed = l;
                    break;
                }
                // Count and age the hits the way route() does.
                ++tick;
                auto & p = pools[layers[l].pool];
                for (uint32_t k = 0; k < K; ++k) {
                    uint32_t slot = p.where[int64_t(l) * E + ids[size_t(l) * K + k]];
                    p.used[slot] = tick;
                    if (p.ahead[slot]) p.ahead[slot] = 0, ++prefetch_hits;
                }
                hits += K;
            }
            if (missed == L) {
                if (logits) ggml_backend_tensor_get(out, logits, 0, size_t(cfg.vocab) * sizeof(float));
                break;
            }
            ++spec_restarts;
            ++restarts_now;
            route(missed, std::vector<int32_t>(ids.begin() + size_t(missed) * K, ids.begin() + size_t(missed + 1) * K), 1);
            start = missed + 1;
            resume = true;
        }
        restart_rate = 0.9 * restart_rate + 0.1 * restarts_now;
        position += 1;
    }

    // The whole-model pass pays off only when restarts are rare: every routed expert must
    // almost always be resident. Measured on a 35B MoE: equal speed with no restarts, a
    // loss as soon as a few layers per token miss. So it runs while the pools hold every
    // expert and the recent restart rate stays low; otherwise decoding goes layer by layer
    // (with next-layer prefetch). After a cool-down it is tried again.
    double restart_rate = 0.0; // exponential average of restarts per whole-pass token
    uint32_t layerwise_left = 0;

    bool whole_pass_ok() {
        static const bool off = std::getenv("LOOM_LMX_LAYERWISE") != nullptr;
        if (off || warm_done.load() < warm_plan.size() || released || budget < expert_bytes) return false;
        if (layerwise_left) return --layerwise_left, false;
        if (restart_rate > 0.25) {
            restart_rate = 0.0;
            layerwise_left = 256;
            return false;
        }
        return true;
    }

    // One forward pass over n <= chunk tokens; writes the last token's logits when asked.
    void forward(const int32_t * tokens, int64_t n, float * logits) {
        if (position + n > context) throw std::runtime_error("context is full");
        if (n == 1 && whole_pass_ok()) return forward_whole(tokens[0], logits);
        const uint32_t L = cfg.layers;
        const int64_t n_kv = std::min<int64_t>(pad_to(uint32_t(position + n), 256), context);

        std::vector<float> embd;
        embed(tokens, n, embd);
        std::vector<int32_t> pos(size_t(n) * 4, 0);
        for (int64_t i = 0; i < n; ++i) pos[i] = pos[n + i] = pos[2 * n + i] = int32_t(position + i);
        std::vector<ggml_fp16_t> mask(size_t(n_kv) * n);
        const ggml_fp16_t zero = ggml_fp32_to_fp16(0.0f), ninf = ggml_fp32_to_fp16(-INFINITY);
        for (int64_t i = 0; i < n; ++i)
            for (int64_t j = 0; j < n_kv; ++j) mask[size_t(i) * n_kv + j] = j <= int64_t(position) + i ? zero : ninf;

        std::vector<int32_t> ids(size_t(std::max(cfg.active, 1u)) * n);
        using clock = std::chrono::steady_clock;
        auto lap = [](clock::time_point & t) {
            auto now = clock::now();
            double d = std::chrono::duration<double, std::micro>(now - t).count();
            t = now;
            return d;
        };
        // One graph per routed layer: it finishes the previous routed layer's experts, runs
        // the dense layers that follow, and ends with the next router, whose choice the CPU
        // needs before the experts can be paged in. Dense models run in one graph.
        uint32_t l = 0;
        int64_t routed = -1; // routed layer whose experts open the next graph
        for (;;) {
            auto t0 = clock::now();
            Graph gr;
            begin(gr);
            ggml_context * c = gr.ctx;
            ggml_tensor * in_embd = nullptr;
            ggml_tensor * x;
            if (routed < 0) {
                in_embd = ggml_new_tensor_2d(c, GGML_TYPE_F32, cfg.hidden, n);
                ggml_set_input(in_embd);
                x = in_embd;
            } else {
                x = experts_ffn(c, uint32_t(routed), n);
            }
            // Unused (and unallocated) when no attention layer is in this graph.
            ggml_tensor * in_pos = ggml_new_tensor_1d(c, GGML_TYPE_I32, n * 4);
            ggml_tensor * in_mask = ggml_new_tensor_2d(c, GGML_TYPE_F16, n_kv, n);
            ggml_set_input(in_pos);
            ggml_set_input(in_mask);
            const uint32_t first = l;
            ggml_tensor * out = nullptr, * pred_t = nullptr;
            for (; l < L && layers[l].dense; ++l) x = dense_layer(c, gr.g, l, x, n, in_pos, in_mask, n_kv);
            const bool routes = l < L;
            if (routes) {
                bool predict = n == 1 && cfg.prefetch > 0 && l + 1 < L && !layers[l + 1].dense;
                out = attention_and_router(c, gr.g, l, x, n, in_pos, in_mask, n_kv, predict ? &pred_t : nullptr);
            } else {
                ggml_tensor * xl = ggml_view_2d(c, x, cfg.hidden, 1, x->nb[1], size_t(n - 1) * x->nb[1]);
                out = ggml_mul_mat(c, output, norm(c, xl, output_norm));
                ggml_set_output(out);
                ggml_build_forward_expand(gr.g, out);
            }
            prof[0] += lap(t0);
            ggml_backend_sched_reset(sched);
            if (!ggml_backend_sched_alloc_graph(sched, gr.g)) throw std::runtime_error("allocate graph");
            prof[1] += lap(t0);
            if (in_embd) ggml_backend_tensor_set(in_embd, embd.data(), 0, embd.size() * sizeof(float));
            if (in_pos->buffer) ggml_backend_tensor_set(in_pos, pos.data(), 0, pos.size() * sizeof(int32_t));
            if (in_mask->buffer) ggml_backend_tensor_set(in_mask, mask.data(), 0, mask.size() * sizeof(ggml_fp16_t));
            compute(gr);
            prof[2] += lap(t0);
            // The pass is valid: commit the recurrent state of the layers it ran.
            for (uint32_t m = first; m < std::min(l + 1, L); ++m) layers[m].cur ^= 1;
            if (!routes) {
                if (logits) ggml_backend_tensor_get(out, logits, 0, size_t(cfg.vocab) * sizeof(float));
                break;
            }
            ggml_backend_tensor_get(out, ids.data(), 0, ids.size() * sizeof(int32_t));
            route(l, ids, n);
            if (pred_t) {
                pred.resize(size_t(pred_t->ne[0]));
                ggml_backend_tensor_get(pred_t, pred.data(), 0, pred.size() * sizeof(int32_t));
                prefetch(l + 1, pred);
            }
            prof[3] += lap(t0);
            routed = l++;
        }
        position += uint32_t(n);
    }

    // ---- memory pressure ----------------------------------------------------------------
    //
    // The automatic budget takes what is free at load. When other programs need memory
    // later, least recently used slots are handed back to the OS (their pages released,
    // the slot disabled); when memory is free again they come back. No reallocation.

    std::chrono::steady_clock::time_point last_adapt{};

    void release_slot(Pool & p, uint32_t s) {
        if (p.owner[s] != kNoOwner) p.where.erase(p.owner[s]);
        p.owner[s] = kNoOwner;
        p.ahead[s] = 0;
        p.off[s] = 1;
        --p.enabled;
#ifndef _WIN32
        static const size_t page = size_t(sysconf(_SC_PAGESIZE));
        for (ggml_tensor * w : p.w) {
            uintptr_t begin = reinterpret_cast<uintptr_t>(w->data) + size_t(s) * w->nb[2];
            uintptr_t end = begin + w->nb[2];
            begin = (begin + page - 1) / page * page;
            end = end / page * page;
#ifdef __APPLE__
            if (end > begin) madvise(reinterpret_cast<void *>(begin), end - begin, MADV_FREE_REUSABLE);
#else
            if (end > begin) madvise(reinterpret_cast<void *>(begin), end - begin, MADV_DONTNEED);
#endif
        }
#endif
    }

    void restore_slot(Pool & p, uint32_t s) {
#ifdef __APPLE__
        static const size_t page = size_t(sysconf(_SC_PAGESIZE));
        for (ggml_tensor * w : p.w) {
            uintptr_t begin = reinterpret_cast<uintptr_t>(w->data) + size_t(s) * w->nb[2];
            uintptr_t end = begin + w->nb[2];
            begin = (begin + page - 1) / page * page;
            end = end / page * page;
            if (end > begin) madvise(reinterpret_cast<void *>(begin), end - begin, MADV_FREE_REUSE);
        }
#endif
        p.off[s] = 0;
        p.used[s] = 0;
        ++p.enabled;
    }

    void adapt_memory() {
        if (!slots_host || cfg.expert_budget) return; // a budget the caller set is kept
        auto now = std::chrono::steady_clock::now();
        if (now - last_adapt < std::chrono::seconds(2)) return;
        last_adapt = now;
        uint64_t total = 0, available = 0;
        system_memory(total, available);
        if (!total || !available) return;
        constexpr uint64_t GiB = 1ull << 30;
        const uint64_t low = std::max<uint64_t>(GiB, total / 20), high = low + 3 * GiB;
        const uint32_t floor = cfg.active * 4; // a bank never drops below a few tokens' experts
        if (available < low) {
            finish_prefetch();
            uint64_t need = low + GiB - available, freed = 0;
            std::vector<std::tuple<uint64_t, uint32_t, uint32_t>> lru;
            for (uint32_t pi = 0; pi < pools.size(); ++pi)
                for (uint32_t s = 0; s < pools[pi].slots; ++s)
                    if (!pools[pi].off[s] && !warming(pools[pi], s)) lru.emplace_back(pools[pi].used[s], pi, s);
            std::sort(lru.begin(), lru.end());
            for (auto & [used, pi, s] : lru) {
                if (freed >= need) break;
                Pool & p = pools[pi];
                if (p.enabled <= floor) continue;
                release_slot(p, s);
                freed += slot_bytes(p);
                released += slot_bytes(p);
            }
        } else if (available > high) {
            uint64_t room = available - high;
            for (auto & p : pools)
                for (uint32_t s = 0; s < p.slots && room >= slot_bytes(p); ++s)
                    if (p.off[s]) {
                        restore_slot(p, s);
                        room -= slot_bytes(p);
                        released -= std::min(released, slot_bytes(p));
                    }
        }
    }

    // Prefill chunk the enabled slots allow: a layer routes to at most E distinct experts.
    int64_t chunk_now() const {
        int64_t n = chunk;
        for (auto & p : pools)
            if (p.enabled < cfg.experts) n = std::min<int64_t>(n, std::max<uint32_t>(1, p.enabled / cfg.active));
        return n;
    }

    void eval(const int32_t * tokens, int64_t count, float * logits) {
        adapt_memory();
        const int64_t step = chunk_now();
        for (int64_t at = 0; at < count; at += step) {
            int64_t n = std::min<int64_t>(step, count - at);
            forward(tokens + at, n, at + n == count ? logits : nullptr);
        }
    }

    // Recurrent state (conv + DeltaNet) at the current position. Attention KV needs no
    // copy: positions below a checkpoint are never rewritten until the caller rolls back
    // to it, and rolling back only moves the position.
    struct Checkpoint {
        uint32_t position = 0;
        std::vector<std::vector<uint8_t>> state;
    };
    std::vector<Checkpoint> checkpoints;

    std::vector<ggml_tensor *> recurrent() const {
        std::vector<ggml_tensor *> out;
        for (auto & lay : layers)
            if (lay.linear) out.push_back(lay.conv[lay.cur]), out.push_back(lay.ssm[lay.cur]);
            else if (lay.recurrent) out.push_back(lay.conv[lay.cur]);
        return out;
    }

    int checkpoint() {
        finish_prefetch();
        Checkpoint cp;
        cp.position = position;
        for (ggml_tensor * t : recurrent()) {
            cp.state.emplace_back(ggml_nbytes(t));
            ggml_backend_tensor_get(t, cp.state.back().data(), 0, ggml_nbytes(t));
        }
        checkpoints.push_back(std::move(cp));
        return int(checkpoints.size() - 1);
    }

    void rollback(int id) {
        if (id < 0 || size_t(id) >= checkpoints.size() || checkpoints[id].state.empty())
            throw std::runtime_error("no such checkpoint");
        finish_prefetch();
        auto ts = recurrent();
        auto & cp = checkpoints[id];
        for (size_t i = 0; i < ts.size(); ++i) ggml_backend_tensor_set(ts[i], cp.state[i].data(), 0, cp.state[i].size());
        position = cp.position;
    }

    // ---- sequence state on disk ---------------------------------------------------------
    //
    // KV rows below the position of every full-attention layer plus the recurrent state.
    // Lets a prompt prefix that every session shares (system prompt and tool definitions)
    // be restored instead of recomputed. The caller keys files by model, settings and tokens.

    static std::FILE * open_file(const char * path, const char * mode) {
#ifdef _WIN32
        auto wide = [](const char * s) {
            int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, nullptr, 0);
            std::wstring w(n, L'\0');
            MultiByteToWideChar(CP_UTF8, 0, s, -1, w.data(), n);
            return w;
        };
        return _wfopen(wide(path).c_str(), wide(mode).c_str());
#else
        return std::fopen(path, mode);
#endif
    }

    struct StateHeader {
        char magic[8];
        uint32_t version, position, layers, kv_type;
        uint32_t kv_rotate, head_dim, kv_heads, pad;
    };

    StateHeader state_header() const {
        StateHeader h{};
        std::memcpy(h.magic, "LOOMSEQ\0", 8);
        h.version = 1, h.position = position, h.layers = cfg.layers, h.kv_type = uint32_t(cfg.kv_type);
        h.kv_rotate = rot != nullptr, h.head_dim = cfg.head_dim, h.kv_heads = cfg.kv_heads;
        return h;
    }

    void save_state(const char * path) {
        finish_prefetch();
        std::FILE * f = open_file(path, "wb");
        if (!f) throw std::runtime_error("cannot write sequence state");
        bool ok = true;
        StateHeader h = state_header();
        ok = ok && std::fwrite(&h, sizeof h, 1, f) == 1;
        std::vector<uint8_t> buf;
        auto put = [&](ggml_tensor * t, size_t bytes) {
            buf.resize(bytes);
            ggml_backend_tensor_get(t, buf.data(), 0, bytes);
            ok = ok && std::fwrite(buf.data(), 1, bytes, f) == bytes;
        };
        for (auto & lay : layers) {
            if (lay.recurrent) {
                put(lay.conv[lay.cur], ggml_nbytes(lay.conv[lay.cur]));
                if (lay.linear) put(lay.ssm[lay.cur], ggml_nbytes(lay.ssm[lay.cur]));
            } else {
                put(lay.k_cache, lay.k_cache->nb[2] * position);
                put(lay.v_cache, lay.v_cache->nb[2] * position);
            }
        }
        ok = std::fclose(f) == 0 && ok;
        if (!ok) throw std::runtime_error("cannot write sequence state");
    }

    // Replaces the sequence with the saved one. False when the file was saved with other
    // settings (the caller then recomputes).
    bool load_state(const char * path) {
        finish_prefetch();
        std::FILE * f = open_file(path, "rb");
        if (!f) return false;
        StateHeader h{}, want = state_header();
        bool ok = std::fread(&h, sizeof h, 1, f) == 1 && !std::memcmp(h.magic, want.magic, 8) && h.version == 1 &&
                  h.layers == want.layers && h.kv_type == want.kv_type && h.kv_rotate == want.kv_rotate &&
                  h.head_dim == want.head_dim && h.kv_heads == want.kv_heads && h.position > 0 && h.position <= context;
        std::vector<uint8_t> buf;
        auto get = [&](ggml_tensor * t, size_t bytes) {
            if (!ok) return;
            buf.resize(bytes);
            ok = std::fread(buf.data(), 1, bytes, f) == bytes;
            if (ok) ggml_backend_tensor_set(t, buf.data(), 0, bytes);
        };
        if (ok) {
            reset();
            for (auto & lay : layers) {
                if (lay.recurrent) {
                    get(lay.conv[lay.cur], ggml_nbytes(lay.conv[lay.cur]));
                    if (lay.linear) get(lay.ssm[lay.cur], ggml_nbytes(lay.ssm[lay.cur]));
                } else {
                    get(lay.k_cache, lay.k_cache->nb[2] * h.position);
                    get(lay.v_cache, lay.v_cache->nb[2] * h.position);
                }
            }
        }
        std::fclose(f);
        if (!ok) {
            reset();
            return false;
        }
        position = h.position;
        return true;
    }

    void forget(int id) {
        if (id >= 0 && size_t(id) < checkpoints.size()) checkpoints[id] = Checkpoint{};
        while (!checkpoints.empty() && checkpoints.back().state.empty()) checkpoints.pop_back();
    }

    void stats(LoomLmxStats & s) const {
        s.hits = hits;
        s.misses = misses;
        s.bytes_read = bytes_read + warm_bytes.load();
        s.resident_bytes = wbuf ? ggml_backend_buffer_get_size(wbuf) : 0;
        s.pool_bytes = 0;
        for (auto & sb : slot_buffers) s.pool_bytes += ggml_backend_buffer_get_size(sb.second);
        s.state_bytes = sbuf ? ggml_backend_buffer_get_size(sbuf) : 0;
        s.slots = 0;
        for (auto & p : pools) s.slots += p.slots;
        s.pools = uint32_t(pools.size());
        s.position = position;
        s.chunk = chunk;
        std::snprintf(s.backend, sizeof s.backend, "%s", backend_name.c_str());
        s.prefetched = prefetched, s.prefetch_hits = prefetch_hits;
        s.budget = budget;
        s.all_experts = expert_bytes;
        s.warm_total = warm_plan.size();
        s.warm_done = warm_done.load();
        s.released = released;
        s.context = context;
        s.spec_tokens = spec_tokens, s.spec_restarts = spec_restarts;
        s.build_us = prof[0], s.alloc_us = prof[1], s.compute_us = prof[2], s.route_us = prof[3];
    }
};

void set_error(char * err, size_t len, const char * what) {
    if (err && len) std::snprintf(err, len, "%s", what);
}

} // namespace

extern "C" {

void * loom_lmx_open(const char * path, const LoomLmxConfig * cfg, const LoomLmxTensor * tensors, size_t count,
                     char * err, size_t err_len) {
    try {
        if (cfg->kv_type < 0 || cfg->kv_type >= GGML_TYPE_COUNT || ggml_blck_size(ggml_type(cfg->kv_type)) <= 0 ||
            cfg->head_dim % ggml_blck_size(ggml_type(cfg->kv_type)) != 0)
            throw std::runtime_error("unsupported KV cache type");
        if (!cfg->mixer || !cfg->dense || !cfg->layers || cfg->active > cfg->experts || (cfg->experts && !cfg->active))
            throw std::runtime_error("invalid engine config");
        auto * e = new Engine(path, *cfg);
        try {
            e->mixer.assign(cfg->mixer, cfg->mixer + cfg->layers);
            e->dense.assign(cfg->dense, cfg->dense + cfg->layers);
            e->cfg.mixer = e->cfg.dense = nullptr;
            e->init_backends();
            e->load(tensors, count);
            e->allocate_state();
        } catch (...) {
            delete e;
            throw;
        }
        return e;
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return nullptr;
    }
}

int loom_lmx_eval(void * engine, const int32_t * tokens, size_t count, float * logits, char * err, size_t err_len) {
    try {
        static_cast<Engine *>(engine)->eval(tokens, int64_t(count), logits);
        return 0;
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return -1;
    }
}

void loom_lmx_reset(void * engine) { static_cast<Engine *>(engine)->reset(); }

// Saves the sequence state at the current position; returns its id, or -1.
int loom_lmx_checkpoint(void * engine, char * err, size_t err_len) {
    try {
        return static_cast<Engine *>(engine)->checkpoint();
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return -1;
    }
}

// Returns the sequence to a checkpoint's position and state. Checkpoints past that
// position become stale once new tokens are evaluated; the caller drops them.
int loom_lmx_rollback(void * engine, int id, char * err, size_t err_len) {
    try {
        static_cast<Engine *>(engine)->rollback(id);
        return 0;
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return -1;
    }
}

void loom_lmx_forget(void * engine, int id) { static_cast<Engine *>(engine)->forget(id); }

int loom_lmx_save_state(void * engine, const char * path, char * err, size_t err_len) {
    try {
        static_cast<Engine *>(engine)->save_state(path);
        return 0;
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return -1;
    }
}

// 1 restored, 0 not usable (missing or other settings; the sequence is then empty), -1 error.
int loom_lmx_load_state(void * engine, const char * path, char * err, size_t err_len) {
    try {
        return static_cast<Engine *>(engine)->load_state(path) ? 1 : 0;
    } catch (const std::exception & ex) {
        set_error(err, err_len, ex.what());
        return -1;
    }
}

void loom_lmx_stats(void * engine, LoomLmxStats * out) { static_cast<Engine *>(engine)->stats(*out); }

void loom_lmx_free(void * engine) { delete static_cast<Engine *>(engine); }

} // extern "C"
