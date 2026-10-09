// Loom's MLX engine: the decoder-only transformer pieces Loom runs, nothing else.
// Rust owns configuration, tokenization, prompts and parsing; this file owns the
// weights, the KV cache and one forward pass per call.
#include <mlx/mlx.h>

#include <cmath>
#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <memory>
#include <optional>
#include <sstream>
#include <string>
#include <unordered_map>
#include <vector>
#include <algorithm>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace mx = mlx::core;

extern "C" {
struct LoomMlxConfig {
  int32_t hidden;
  int32_t layers;
  int32_t heads;
  int32_t kv_heads;
  int32_t head_dim;
  float eps;
  float rope_theta;
  int32_t tied_embeddings;
  int32_t qk_norm;
  int32_t group_size;
  int32_t bits;  // 0: weights are not quantized
  int32_t prefill_chunk;
  int32_t kv_bits;  // 0: bf16 keys and values; 8 or 4: quantized per kv_group values
  int32_t kv_group;
  int32_t kv_rotate;  // TurboQuant-style random Hadamard rotation before quantizing
  int32_t experts;
  int32_t experts_per_token;
  int32_t norm_topk_prob;
};
}

namespace {

using Weights = std::unordered_map<std::string, mx::array>;

// A lazy safetensors loader can retain this reader after loom_mlx_load returns.
// Every positional read is confined to one LMX payload; no weight extraction.
class SliceReader final : public mx::io::Reader {
 public:
  SliceReader(std::string file, uint64_t base, uint64_t size)
      : path_(std::move(file)), base_(base), size_(size) {
    fd_ = ::open(path_.c_str(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    struct stat info{};
    if (fd_ < 0 || fstat(fd_, &info) != 0 || !S_ISREG(info.st_mode) ||
        base > INT64_MAX || size > INT64_MAX - base || base + size > uint64_t(info.st_size)) {
      if (fd_ >= 0) ::close(fd_);
      fd_ = -1;
      throw std::runtime_error("invalid LMX file slice");
    }
  }
  ~SliceReader() override { if (fd_ >= 0) ::close(fd_); }
  bool is_open() const override { return fd_ >= 0; }
  bool good() const override { return is_open(); }
  size_t tell() override { return cursor_; }
  void seek(int64_t offset, std::ios_base::seekdir way) override {
    int64_t origin = way == std::ios_base::beg ? 0 : way == std::ios_base::end ? int64_t(size_) : int64_t(cursor_);
    if ((offset > 0 && origin > INT64_MAX - offset) || (offset < 0 && offset < -origin)) {
      throw std::runtime_error("LMX slice seek out of bounds");
    }
    auto position = origin + offset;
    if (position < 0 || uint64_t(position) > size_) throw std::runtime_error("LMX slice seek out of bounds");
    cursor_ = size_t(position);
  }
  void read(char* data, size_t n) override { read(data, n, cursor_); cursor_ += n; }
  void read(char* data, size_t n, size_t offset) override {
    if (offset > size_ || n > size_ - offset) throw std::runtime_error("LMX slice read out of bounds");
    while (n) {
      auto count = ::pread(fd_, data, std::min(n, size_t(INT32_MAX)), off_t(base_ + offset));
      if (count < 0 && errno == EINTR) continue;
      if (count <= 0) throw std::runtime_error("truncated LMX slice read");
      data += count;
      n -= size_t(count);
      offset += size_t(count);
    }
  }
  std::string label() const override { return "LMX slice of " + path_; }
 private:
  int fd_ = -1;
  std::string path_;
  uint64_t base_, size_;
  size_t cursor_ = 0;
};

mx::SafetensorsLoad load_weight_file(const std::string& path) {
  if (path.rfind("lmx:", 0) != 0) return mx::load_safetensors(path);
  auto first = path.find(':', 4), second = first == std::string::npos ? first : path.find(':', first + 1);
  if (first == std::string::npos || second == std::string::npos) throw std::runtime_error("invalid LMX slice descriptor");
  auto number = [](const std::string& value) {
    if (value.empty() || value.find_first_not_of("0123456789") != std::string::npos) throw std::runtime_error("invalid LMX slice number");
    return std::stoull(value);
  };
  return mx::load_safetensors(std::make_shared<SliceReader>(path.substr(second + 1),
      number(path.substr(4, first - 4)), number(path.substr(first + 1, second - first - 1))));
}

mx::array take(Weights& weights, const std::string& name) {
  auto found = weights.find(name);
  if (found == weights.end()) {
    throw std::runtime_error("missing weight " + name);
  }
  return found->second;
}

std::optional<mx::array> maybe(Weights& weights, const std::string& name) {
  auto found = weights.find(name);
  if (found == weights.end()) {
    return std::nullopt;
  }
  return found->second;
}

mx::array swiglu(const mx::array& gate, const mx::array& up) {
  // Elementwise fusion avoids intermediate activation buffers, without copying
  // projection weights. The shapeless graph serves dense and selected MoE rows.
  static const bool enabled = [] {
    const char* value = std::getenv("LOOM_MLX_COMPILE_ACTIVATIONS");
    return !value || std::strcmp(value, "0") != 0;
  }();
  if (!enabled) return gate * mx::sigmoid(gate) * up;
  static auto compiled = mx::compile([](const std::vector<mx::array>& inputs) {
    return std::vector<mx::array>{inputs[0] * mx::sigmoid(inputs[0]) * inputs[1]};
  }, true);
  return compiled({gate, up})[0];
}

struct Linear {
  std::optional<mx::array> weight, scales, biases, bias;
  int group_size = 0;
  int bits = 0;

  static Linear load(Weights& weights, const std::string& prefix, const LoomMlxConfig& config) {
    Linear linear;
    linear.weight = take(weights, prefix + ".weight");
    linear.scales = maybe(weights, prefix + ".scales");
    linear.biases = maybe(weights, prefix + ".biases");
    linear.bias = maybe(weights, prefix + ".bias");
    linear.group_size = config.group_size;
    linear.bits = config.bits;
    if (linear.scales) {
      // A checkpoint may keep the router at higher precision than its experts.
      // Infer affine packing per tensor, rather than assuming one model-wide bit rate.
      int input = linear.scales->shape(-1) * linear.group_size;
      int packed_bits = linear.weight->shape(-1) * 32;
      if (input <= 0 || packed_bits % input != 0) {
        throw std::runtime_error("unsupported quantization layout: " + prefix);
      }
      linear.bits = packed_bits / input;
    }
    return linear;
  }

  mx::array operator()(const mx::array& x) const {
    auto y = scales
        ? mx::quantized_matmul(x, *weight, *scales, biases, true, group_size, bits)
        : mx::matmul(x, mx::transpose(*weight));
    return bias ? y + *bias : y;
  }

  // Rows of an embedding table, dequantized only for the requested tokens.
  mx::array rows(const mx::array& ids) const {
    if (!scales) {
      return mx::take(*weight, ids, 0);
    }
    return mx::dequantize(
        mx::take(*weight, ids, 0),
        mx::take(*scales, ids, 0),
        biases ? std::optional<mx::array>(mx::take(*biases, ids, 0)) : std::nullopt,
        group_size,
        bits);
  }

  // Expert weights remain packed [experts, out, packed_in]. Gather only the
  // selected matrix products, never construct a dequantized expert bank.
  mx::array expert(const mx::array& x, const mx::array& indices) const {
    auto y = scales
        ? mx::gather_qmm(x, *weight, *scales, biases, std::nullopt, indices,
            true, group_size, bits)
        : mx::gather_mm(x, mx::swapaxes(*weight, -1, -2), std::nullopt, indices);
    return bias ? y + mx::expand_dims(mx::take(*bias, indices, 0), -2) : y;
  }
};

struct Layer {
  std::optional<mx::array> input_norm, post_norm, q_norm, k_norm;
  Linear q, k, v, o, gate, up, down;
  std::optional<Linear> qkv;
  std::optional<Linear> router;

  mx::array mlp(const mx::array& x, const LoomMlxConfig& config) const {
    if (!router) {
      auto g = gate(x);
      return down(swiglu(g, up(x)));
    }
    // Qwen3 MoE uses softmax routing, top-k selection, optional renormalization
    // and a weighted SwiGLU expert sum. See mlx-lm/models/qwen3_moe.py.
    auto probabilities = mx::softmax((*router)(x), -1, true);
    int k = config.experts_per_token;
    auto partition = mx::argpartition(probabilities, -k, -1);
    auto indices = mx::slice(partition, {0, 0, config.experts - k},
        {x.shape(0), x.shape(1), config.experts});
    auto scores = mx::take_along_axis(probabilities, indices, -1);
    if (config.norm_topk_prob) {
      scores = scores / mx::sum(scores, -1, true);
    }
    auto expanded = mx::expand_dims(mx::expand_dims(x, -2), -2);
    auto g = gate.expert(expanded, indices);
    auto u = up.expert(expanded, indices);
    auto y = mx::squeeze(down.expert(swiglu(g, u), indices), -2);
    return mx::astype(mx::sum(y * mx::expand_dims(scores, -1), -2), x.dtype());
  }
};

struct Model {
  LoomMlxConfig config;
  Linear embed, head;
  std::optional<mx::array> norm;
  std::vector<Layer> layers;
  // KV cache: one [1, kv_heads, capacity, head_dim] buffer per layer; quantized caches
  // keep packed values plus per-group scales and biases.
  std::vector<std::optional<mx::array>> keys, values;
  std::vector<std::vector<mx::array>> qkeys, qvalues;
  std::optional<mx::array> signs;  // random ±1 per head dimension, fixed per model
  int offset = 0;
  std::vector<int32_t> tokens;
  int top_k = 0;
  float top_p = 1.0f;
  float repetition = 1.0f;

  void rewind(size_t keep) {
    if (keep == 0) {
      // A new conversation must not retain a previous conversation's large KV
      // capacity. Prefix reuse and speculative rollback keep their buffers.
      for (auto& key : keys) key.reset();
      for (auto& value : values) value.reset();
      for (auto& parts : qkeys) parts.clear();
      for (auto& parts : qvalues) parts.clear();
      offset = 0;
    } else {
      offset = static_cast<int>(std::min(static_cast<size_t>(offset), keep));
    }
    tokens.resize(offset);
  }

  mx::array sample(mx::array logits, float temperature, uint64_t seed, size_t context) const {
    if (temperature <= 0.0f) return mx::argmax(logits, -1);
    logits = mx::astype(mx::reshape(logits, {-1}), mx::float32);
    int vocab = logits.size();
    if (repetition != 1.0f && context) {
      std::vector<int32_t> seen(tokens.begin(), tokens.begin() + std::min(context, tokens.size()));
      std::sort(seen.begin(), seen.end());
      seen.erase(std::unique(seen.begin(), seen.end()), seen.end());
      auto indices = mx::array(seen.data(), {static_cast<int>(seen.size())}, mx::int32);
      auto selected = mx::take(logits, indices);
      selected = mx::where(selected < 0, selected * repetition, selected / repetition);
      logits = mx::put_along_axis(logits, indices, selected, -1);
    }
    logits = logits / temperature;
    std::optional<mx::array> candidates;
    if (top_k > 0 && top_k < vocab) {
      auto partition = mx::argpartition(-logits, top_k - 1);
      candidates = mx::slice(partition, {0}, {top_k});
      logits = mx::take(logits, *candidates);
    }
    if (top_p < 1.0f) {
      auto order = mx::argsort(-logits);
      logits = mx::take(logits, order);
      candidates = candidates ? mx::take(*candidates, order) : order;
      auto probabilities = mx::softmax(logits, -1, true);
      auto preceding = mx::cumsum(probabilities) - probabilities;
      logits = mx::where(preceding < top_p, logits, mx::array(-std::numeric_limits<float>::infinity()));
    }
    auto chosen = mx::random::categorical(logits, 0, mx::random::key(seed));
    return candidates ? mx::take(*candidates, chosen) : chosen;
  }

  // Orthonormal R = H·S: spreads outlier channels so low-bit groups lose less.
  mx::array rotate(const mx::array& x) const {
    auto scale = 1.0f / std::sqrt(static_cast<float>(config.head_dim));
    return mx::hadamard_transform(mx::multiply(x, *signs), scale);
  }
  // Rᵀ = S·H, since H is symmetric.
  mx::array unrotate(const mx::array& x) const {
    auto scale = 1.0f / std::sqrt(static_cast<float>(config.head_dim));
    return mx::multiply(mx::hadamard_transform(x, scale), *signs);
  }

  // Append to one quantized cache part, growing it like the plain cache.
  static mx::array append(std::optional<mx::array> stored, const mx::array& part, int offset, int needed) {
    auto shape = part.shape();
    int width = shape[3];
    if (!stored || needed > stored->shape(2)) {
      int capacity = ((needed + 255) / 256) * 256;
      auto grown = mx::zeros(mx::Shape{shape[0], shape[1], capacity, width}, part.dtype());
      if (stored && offset > 0) {
        mx::Shape start = {0, 0, 0, 0};
        mx::Shape stop = {shape[0], shape[1], offset, width};
        grown = mx::slice_update(grown, mx::slice(*stored, start, stop), start, stop);
      }
      stored = grown;
    }
    mx::Shape start = {0, 0, offset, 0};
    mx::Shape stop = {shape[0], shape[1], needed, width};
    return mx::slice_update(*stored, part, start, stop);
  }

  // Attention over a quantized cache: q·Kᵀ and p·V straight from packed values.
  mx::array quantized_attention(int layer, const mx::array& q, const mx::array& k, const mx::array& v, float scale) {
    const auto& c = config;
    int length = q.shape(2);
    int needed = offset + length;
    bool rotated = c.kv_rotate && signs;
    auto packed_keys = mx::quantize(rotated ? rotate(k) : k, c.kv_group, c.kv_bits);
    auto packed_values = mx::quantize(rotated ? rotate(v) : v, c.kv_group, c.kv_bits);
    auto& stored_keys = qkeys[layer];
    auto& stored_values = qvalues[layer];
    for (int part = 0; part < 3; ++part) {
      std::optional<mx::array> previous_key, previous_value;
      if (stored_keys.size() == 3) {
        previous_key = stored_keys[part];
        previous_value = stored_values[part];
      }
      auto key = append(previous_key, packed_keys[part], offset, needed);
      auto value = append(previous_value, packed_values[part], offset, needed);
      if (stored_keys.size() < 3) {
        stored_keys.push_back(key);
        stored_values.push_back(value);
      } else {
        stored_keys[part] = key;
        stored_values[part] = value;
      }
    }
    auto visible = [&](const mx::array& part) {
      mx::Shape start = {0, 0, 0, 0};
      mx::Shape stop = {1, c.kv_heads, needed, part.shape(3)};
      auto view = mx::slice(part, start, stop);
      return mx::expand_dims(view, 2);  // [1, kv_heads, 1, needed, width]
    };
    int repeats = c.heads / c.kv_heads;
    auto scaled = mx::multiply(rotated ? rotate(q) : q, mx::array(scale, q.dtype()));
    auto grouped = mx::reshape(scaled, mx::Shape{1, c.kv_heads, repeats, length, c.head_dim});
    auto scores = mx::quantized_matmul(grouped, visible(stored_keys[0]), visible(stored_keys[1]),
        visible(stored_keys[2]), true, c.kv_group, c.kv_bits);
    if (length > 1) {
      auto rows = mx::reshape(mx::arange(offset, needed, mx::int32), mx::Shape{length, 1});
      auto columns = mx::reshape(mx::arange(0, needed, mx::int32), mx::Shape{1, needed});
      scores = mx::where(mx::less_equal(columns, rows), scores,
          mx::array(-std::numeric_limits<float>::infinity(), scores.dtype()));
    }
    auto weights = mx::softmax(scores, -1, true);
    auto attended = mx::quantized_matmul(weights, visible(stored_values[0]), visible(stored_values[1]),
        visible(stored_values[2]), false, c.kv_group, c.kv_bits);
    attended = mx::reshape(attended, mx::Shape{1, c.heads, length, c.head_dim});
    return rotated ? unrotate(attended) : attended;
  }

  std::pair<mx::array, mx::array> remember(int layer, const mx::array& k, const mx::array& v) {
    int length = k.shape(2);
    int needed = offset + length;
    auto& stored_keys = keys[layer];
    auto& stored_values = values[layer];
    if (!stored_keys || needed > stored_keys->shape(2)) {
      // Grow in steps so a long session reallocates rarely.
      int capacity = ((needed + 255) / 256) * 256;
      mx::Shape shape = {1, config.kv_heads, capacity, config.head_dim};
      auto grown_keys = mx::zeros(shape, k.dtype());
      auto grown_values = mx::zeros(shape, v.dtype());
      if (stored_keys && offset > 0) {
        mx::Shape start = {0, 0, 0, 0};
        mx::Shape stop = {1, config.kv_heads, offset, config.head_dim};
        grown_keys = mx::slice_update(grown_keys, mx::slice(*stored_keys, start, stop), start, stop);
        grown_values = mx::slice_update(grown_values, mx::slice(*stored_values, start, stop), start, stop);
      }
      stored_keys = grown_keys;
      stored_values = grown_values;
    }
    mx::Shape start = {0, 0, offset, 0};
    mx::Shape stop = {1, config.kv_heads, needed, config.head_dim};
    stored_keys = mx::slice_update(*stored_keys, k, start, stop);
    stored_values = mx::slice_update(*stored_values, v, start, stop);
    mx::Shape all = {1, config.kv_heads, needed, config.head_dim};
    return {mx::slice(*stored_keys, {0, 0, 0, 0}, all), mx::slice(*stored_values, {0, 0, 0, 0}, all)};
  }

  // Runs `ids` at positions offset.., advancing the cache. Returns the hidden states.
  mx::array forward(const std::vector<int32_t>& ids) {
    int length = static_cast<int>(ids.size());
    const auto& c = config;
    auto tokens = mx::array(ids.data(), {1, length}, mx::int32);
    auto h = embed.rows(tokens);
    float scale = 1.0f / std::sqrt(static_cast<float>(c.head_dim));
    for (size_t index = 0; index < layers.size(); ++index) {
      const auto& layer = layers[index];
      auto x = mx::fast::rms_norm(h, layer.input_norm, c.eps);
      mx::array q(0), k(0), v(0);
      if (layer.qkv) {
        // One packed GEMM replaces three Q/K/V GEMMs. Keep each output's original
        // channel order, including models where query and KV head counts differ.
        auto projected = (*layer.qkv)(x);
        int q_width = c.heads * c.head_dim;
        int kv_width = c.kv_heads * c.head_dim;
        q = mx::reshape(mx::slice(projected, {0, 0, 0}, {1, length, q_width}),
            {1, length, c.heads, c.head_dim});
        k = mx::reshape(mx::slice(projected, {0, 0, q_width}, {1, length, q_width + kv_width}),
            {1, length, c.kv_heads, c.head_dim});
        v = mx::reshape(mx::slice(projected, {0, 0, q_width + kv_width},
                {1, length, q_width + 2 * kv_width}),
            {1, length, c.kv_heads, c.head_dim});
      } else {
        q = mx::reshape(layer.q(x), {1, length, c.heads, c.head_dim});
        k = mx::reshape(layer.k(x), {1, length, c.kv_heads, c.head_dim});
        v = mx::reshape(layer.v(x), {1, length, c.kv_heads, c.head_dim});
      }
      if (layer.q_norm) {
        q = mx::fast::rms_norm(q, layer.q_norm, c.eps);
        k = mx::fast::rms_norm(k, layer.k_norm, c.eps);
      }
      q = mx::transpose(q, {0, 2, 1, 3});
      k = mx::transpose(k, {0, 2, 1, 3});
      v = mx::transpose(v, {0, 2, 1, 3});
      q = mx::fast::rope(q, c.head_dim, false, c.rope_theta, 1.0f, offset);
      k = mx::fast::rope(k, c.head_dim, false, c.rope_theta, 1.0f, offset);
      mx::array attended = c.kv_bits > 0
          ? quantized_attention(static_cast<int>(index), q, k, v, scale)
          : [&] {
              auto [all_keys, all_values] = remember(static_cast<int>(index), k, v);
              return mx::fast::scaled_dot_product_attention(
                  q, all_keys, all_values, scale, length > 1 ? "causal" : "");
            }();
      attended = mx::reshape(mx::transpose(attended, {0, 2, 1, 3}), {1, length, c.heads * c.head_dim});
      h = h + layer.o(attended);
      x = mx::fast::rms_norm(h, layer.post_norm, c.eps);
      h = h + layer.mlp(x, c);
    }
    offset += length;
    this->tokens.insert(this->tokens.end(), ids.begin(), ids.end());
    return h;
  }

  mx::array logits(const mx::array& hidden) {
    int length = hidden.shape(1);
    auto last = mx::slice(hidden, {0, length - 1, 0}, {1, length, config.hidden});
    return head(mx::fast::rms_norm(last, norm, config.eps));
  }
};

void fail(char* error, size_t capacity, const std::string& message) {
  if (error && capacity > 0) {
    std::strncpy(error, message.c_str(), capacity - 1);
    error[capacity - 1] = '\0';
  }
}

// Builds the model from named weights; throws on a malformed or unsupported checkpoint.
void* build_model(Weights& weights, const LoomMlxConfig* config) {
  // Release partially loaded layers on malformed or unsupported checkpoints.
  auto model = std::make_unique<Model>(Model{*config});
  model->embed = Linear::load(weights, "model.embed_tokens", *config);
  model->head = config->tied_embeddings ? model->embed : Linear::load(weights, "lm_head", *config);
  model->norm = take(weights, "model.norm.weight");
  for (int index = 0; index < config->layers; ++index) {
    auto prefix = "model.layers." + std::to_string(index);
    Layer layer;
    layer.input_norm = take(weights, prefix + ".input_layernorm.weight");
    layer.post_norm = take(weights, prefix + ".post_attention_layernorm.weight");
    if (config->qk_norm) {
      layer.q_norm = take(weights, prefix + ".self_attn.q_norm.weight");
      layer.k_norm = take(weights, prefix + ".self_attn.k_norm.weight");
    }
    layer.q = Linear::load(weights, prefix + ".self_attn.q_proj", *config);
    layer.k = Linear::load(weights, prefix + ".self_attn.k_proj", *config);
    layer.v = Linear::load(weights, prefix + ".self_attn.v_proj", *config);
    // MLX's packed format quantizes the input channel axis. Stacking output
    // rows preserves per-group scales/biases and computes all projections in
    // one call. Opt in: concatenation allocates another packed weight buffer.
    const char* fuse_qkv = std::getenv("LOOM_MLX_FUSE_QKV");
    if (fuse_qkv && std::strcmp(fuse_qkv, "1") == 0 &&
        layer.q.scales && layer.k.scales && layer.v.scales &&
        layer.q.bits == layer.k.bits && layer.q.bits == layer.v.bits &&
        layer.q.bias.has_value() == layer.k.bias.has_value() &&
        layer.q.bias.has_value() == layer.v.bias.has_value() &&
        layer.q.biases.has_value() == layer.k.biases.has_value() &&
        layer.q.biases.has_value() == layer.v.biases.has_value()) {
      Linear fused;
      fused.group_size = config->group_size;
      fused.bits = layer.q.bits;
      fused.weight = mx::concatenate({*layer.q.weight, *layer.k.weight, *layer.v.weight}, 0);
      fused.scales = mx::concatenate({*layer.q.scales, *layer.k.scales, *layer.v.scales}, 0);
      if (layer.q.biases && layer.k.biases && layer.v.biases) {
        fused.biases = mx::concatenate({*layer.q.biases, *layer.k.biases, *layer.v.biases}, 0);
      }
      if (layer.q.bias && layer.k.bias && layer.v.bias) {
        fused.bias = mx::concatenate({*layer.q.bias, *layer.k.bias, *layer.v.bias}, 0);
      }
      // Materialize now so the lazy concatenation releases its source buffers
      // as each layer is loaded, instead of retaining both versions until prefill.
      std::vector<mx::array> packed{*fused.weight, *fused.scales};
      if (fused.biases) packed.push_back(*fused.biases);
      if (fused.bias) packed.push_back(*fused.bias);
      mx::eval(packed);
      for (const auto* projection : {"q_proj", "k_proj", "v_proj"}) {
        for (const auto* suffix : {"weight", "scales", "biases", "bias"}) {
          weights.erase(prefix + ".self_attn." + projection + "." + suffix);
        }
      }
      layer.qkv = std::move(fused);
      layer.q = Linear{};
      layer.k = Linear{};
      layer.v = Linear{};
    }
    layer.o = Linear::load(weights, prefix + ".self_attn.o_proj", *config);
    bool sparse = config->experts > 0 &&
        weights.count(prefix + ".mlp.switch_mlp.gate_proj.weight");
    if (sparse) {
      layer.router = Linear::load(weights, prefix + ".mlp.gate", *config);
      if (layer.router->weight->shape(0) != config->experts) {
        throw std::runtime_error("router expert count mismatch: " + prefix);
      }
    }
    auto mlp = prefix + (sparse ? ".mlp.switch_mlp" : ".mlp");
    layer.gate = Linear::load(weights, mlp + ".gate_proj", *config);
    layer.up = Linear::load(weights, mlp + ".up_proj", *config);
    layer.down = Linear::load(weights, mlp + ".down_proj", *config);
    if (sparse && (layer.gate.weight->shape(0) != config->experts ||
        layer.up.weight->shape(0) != config->experts ||
        layer.down.weight->shape(0) != config->experts)) {
      throw std::runtime_error("expert bank size mismatch: " + prefix);
    }
    model->layers.push_back(std::move(layer));
  }
  model->keys.resize(config->layers);
  model->values.resize(config->layers);
  model->qkeys.resize(config->layers);
  model->qvalues.resize(config->layers);
  if (config->kv_rotate) {
    auto bits = mx::random::bernoulli(0.5f, {config->head_dim}, mx::random::key(0x7175));
    model->signs = mx::astype(mx::subtract(mx::multiply(mx::astype(bits, mx::float32), mx::array(2.0f)), mx::array(1.0f)), mx::bfloat16);
  }
  return model.release();
}

}  // namespace

extern "C" {

void loom_mlx_set_sampling(void* handle, int32_t top_k, float top_p, float repetition) {
  auto model = static_cast<Model*>(handle);
  model->top_k = top_k;
  model->top_p = top_p;
  model->repetition = repetition;
}

// `files`: newline-separated safetensors paths. Weights stay lazy (memory-mapped) until
// the first forward pass touches them.
void* loom_mlx_load(const char* files, const LoomMlxConfig* config, char* error, size_t capacity) {
  try {
    Weights weights;
    std::stringstream paths(files);
    std::string path;
    while (std::getline(paths, path)) {
      if (path.empty()) {
        continue;
      }
      auto loaded = load_weight_file(path);
      for (auto& [name, value] : loaded.first) {
        weights.insert_or_assign(name, value);
      }
    }
    return build_model(weights, config);
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return nullptr;
  }
}

// Weights converted in memory (GGUF blocks repacked as MLX affine groups), added one
// tensor at a time so the caller holds at most one source tensor besides the model.
void* loom_mlx_weights_new() { return new Weights(); }

void loom_mlx_weights_free(void* weights) { delete static_cast<Weights*>(weights); }

// Allocates final MLX storage for the caller to fill synchronously before model build.
// dtype: 0 uint32, 1 float16, 2 float32, 3 bfloat16. The pending weights own the buffer.
void* loom_mlx_weights_alloc(void* handle, const char* name, int32_t dtype, const int64_t* dims,
    size_t ndim, size_t bytes, char* error, size_t capacity) {
  try {
    auto& weights = *static_cast<Weights*>(handle);
    if (weights.count(name)) throw std::runtime_error("duplicate weight " + std::string(name));
    mx::Shape shape;
    size_t count = 1;
    for (size_t i = 0; i < ndim; ++i) {
      if (dims[i] <= 0 || dims[i] > INT32_MAX || size_t(dims[i]) > SIZE_MAX / count) {
        throw std::runtime_error("invalid tensor shape");
      }
      shape.push_back(static_cast<int32_t>(dims[i]));
      count *= size_t(dims[i]);
    }
    auto type = dtype == 0 ? mx::uint32 : dtype == 1 ? mx::float16
        : dtype == 2 ? mx::float32 : dtype == 3 ? mx::bfloat16
        : throw std::runtime_error("unknown tensor dtype");
    auto itemsize = mx::size_of(type);
    if (count > SIZE_MAX / itemsize || count * itemsize != bytes) {
      throw std::runtime_error("tensor bytes do not match its shape: " + std::string(name));
    }
    auto free_buffer = [](mx::allocator::Buffer* buffer) { mx::allocator::free(*buffer); delete buffer; };
    std::unique_ptr<mx::allocator::Buffer, decltype(free_buffer)> buffer(
        new mx::allocator::Buffer(mx::allocator::malloc(bytes)), free_buffer);
    if (!buffer->ptr()) throw std::bad_alloc();
    auto data = buffer->raw_ptr();
    // Rust receives an initialized byte slice, then overwrites it during conversion.
    std::memset(data, 0, bytes);
    mx::array array(*buffer, shape, type);
    delete buffer.release();  // The array now owns the allocation, including on insertion failure.
    weights.emplace(name, std::move(array));
    return data;
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return nullptr;
  }
}

// Consumes `weights` whether or not the model builds.
void* loom_mlx_load_weights(void* weights, const LoomMlxConfig* config, char* error, size_t capacity) {
  std::unique_ptr<Weights> owned(static_cast<Weights*>(weights));
  try {
    return build_model(*owned, config);
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return nullptr;
  }
}

// One bounded prefill pass, without sampling. Rust can yield its scratch lease
// and check deadlines between passes instead of holding it for an entire prompt.
int loom_mlx_prefill(
    void* handle, const int32_t* tokens, size_t count, size_t keep,
    char* error, size_t capacity) {
  try {
    if (count == 0) throw std::runtime_error("no tokens to run");
    auto model = static_cast<Model*>(handle);
    model->rewind(keep);
    auto hidden = model->forward(std::vector<int32_t>(tokens, tokens + count));
    std::vector<mx::array> cached;
    for (auto& keys : model->keys) if (keys) cached.push_back(*keys);
    for (auto& values : model->values) if (values) cached.push_back(*values);
    for (auto& parts : model->qkeys) for (auto& part : parts) cached.push_back(part);
    for (auto& parts : model->qvalues) for (auto& part : parts) cached.push_back(part);
    mx::eval(cached);
    return 0;
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return 1;
  }
}

void loom_mlx_reset(void* handle) {
  static_cast<Model*>(handle)->rewind(0);
  mx::clear_cache();
}

void loom_mlx_clear_cache() { mx::clear_cache(); }

// Keep the first `keep` cached positions, run `tokens` after them and sample the next
// token. Greedy when `temperature` <= 0.
int loom_mlx_next(
    void* handle,
    const int32_t* tokens,
    size_t count,
    size_t keep,
    float temperature,
    uint64_t seed,
    int32_t* next,
    char* error,
    size_t capacity) {
  try {
    auto model = static_cast<Model*>(handle);
    if (count == 0) {
      throw std::runtime_error("no tokens to run");
    }
    model->rewind(keep);
    int chunk = model->config.prefill_chunk > 0 ? model->config.prefill_chunk : 512;
    std::vector<int32_t> pending(tokens, tokens + count);
    std::optional<mx::array> hidden;
    for (size_t start = 0; start < pending.size(); start += chunk) {
      size_t stop = std::min(pending.size(), start + chunk);
      hidden = model->forward(std::vector<int32_t>(pending.begin() + start, pending.begin() + stop));
      if (stop < pending.size()) {
        // Materialize the cache per chunk so a long prompt never builds one huge graph.
        std::vector<mx::array> cached;
        for (auto& keys : model->keys) if (keys) cached.push_back(*keys);
        for (auto& values : model->values) if (values) cached.push_back(*values);
        for (auto& parts : model->qkeys) for (auto& part : parts) cached.push_back(part);
        for (auto& parts : model->qvalues) for (auto& part : parts) cached.push_back(part);
        mx::eval(cached);
      }
    }
    auto logits = mx::reshape(model->logits(*hidden), {-1});
    auto token = model->sample(logits, temperature, seed, model->tokens.size());
    mx::eval(token);
    *next = token.item<int32_t>();
    return 0;
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return 1;
  }
}

// Run `count` tokens after the first `keep` cached ones and sample a next token after
// every one of them (`next[i]` follows `tokens[i]`). Speculative drafts are checked by
// comparing each draft with the token sampled at the position before it.
int loom_mlx_verify(
    void* handle,
    const int32_t* tokens,
    size_t count,
    size_t keep,
    float temperature,
    uint64_t seed,
    int32_t* next,
    char* error,
    size_t capacity) {
  try {
    auto model = static_cast<Model*>(handle);
    if (count == 0) {
      throw std::runtime_error("no tokens to run");
    }
    model->rewind(keep);
    auto hidden = model->forward(std::vector<int32_t>(tokens, tokens + count));
    auto logits = model->head(mx::fast::rms_norm(hidden, model->norm, model->config.eps));
    logits = mx::reshape(logits, {static_cast<int>(count), -1});
    auto sampled = mx::argmax(logits, -1);
    if (temperature > 0.0f) {
      std::vector<mx::array> rows;
      for (size_t i = 0; i < count; ++i) {
        rows.push_back(model->sample(mx::take(logits, mx::array(static_cast<int32_t>(i)), 0),
            temperature, seed + i, keep + i + 1));
      }
      sampled = mx::stack(rows);
    }
    sampled = mx::astype(sampled, mx::int32);
    mx::eval(sampled);
    std::memcpy(next, sampled.data<int32_t>(), count * sizeof(int32_t));
    return 0;
  } catch (const std::exception& exception) {
    fail(error, capacity, exception.what());
    return 1;
  }
}

int loom_mlx_available() { return mx::is_available(mx::Device(mx::Device::gpu)) ? 1 : 0; }

size_t loom_mlx_active_memory() {
  return mx::get_active_memory();
}

size_t loom_mlx_peak_memory() {
  return mx::get_peak_memory();
}

size_t loom_mlx_cache_memory() {
  return mx::get_cache_memory();
}

void loom_mlx_reset_peak_memory() {
  mx::reset_peak_memory();
}

// Freed buffers MLX keeps for reuse; capping it trades a little speed for footprint.
void loom_mlx_set_cache_limit(size_t bytes) {
  mx::set_cache_limit(bytes);
}

void loom_mlx_free(void* handle) {
  delete static_cast<Model*>(handle);
  mx::clear_cache();
}

}  // extern "C"
