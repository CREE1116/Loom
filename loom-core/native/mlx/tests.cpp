// Standalone native regression checks; see docs/loom-mlx-memory-moe.md.
#include "engine.cpp"
#include <algorithm>
#include <cstdio>
#include <iostream>
#include <fstream>
#include <numeric>
#include <unistd.h>

namespace {
mx::array fixture(const mx::Shape& shape, float phase) {
  size_t size = 1;
  for (int dim : shape) size *= dim;
  std::vector<float> data(size);
  for (size_t i = 0; i < size; ++i) {
    data[i] = std::sin(i * 0.37f + phase) * 0.08f;
  }
  return mx::array(data.data(), shape, mx::float32);
}

void insert(Weights& weights, const std::string& name, const mx::array& w, int bits) {
  if (bits == 0) {
    weights.insert_or_assign(name + ".weight", w);
  } else {
    auto packed = mx::quantize(w, 64, bits);
    weights.insert_or_assign(name + ".weight", packed[0]);
    weights.insert_or_assign(name + ".scales", packed[1]);
    weights.insert_or_assign(name + ".biases", packed[2]);
  }
}

mx::array unpack(const Linear& linear) {
  return linear.scales
      ? mx::dequantize(*linear.weight, *linear.scales, linear.biases,
          linear.group_size, linear.bits)
      : *linear.weight;
}

void routing_check(int length, int k, bool normalize, int bits, mx::Dtype dtype) {
  LoomMlxConfig config{};
  config.group_size = 64;
  config.bits = bits;
  config.experts = 3;
  config.experts_per_token = k;
  config.norm_topk_prob = normalize;
  Weights weights;
  insert(weights, "router", mx::astype(fixture({3, 64}, 0.1f), dtype), bits ? 8 : 0);
  insert(weights, "gate", mx::astype(fixture({3, 64, 64}, 0.5f), dtype), bits);
  insert(weights, "up", mx::astype(fixture({3, 64, 64}, 1.3f), dtype), bits);
  insert(weights, "down", mx::astype(fixture({3, 64, 64}, 2.1f), dtype), bits);
  Layer layer;
  layer.router = Linear::load(weights, "router", config);
  layer.gate = Linear::load(weights, "gate", config);
  layer.up = Linear::load(weights, "up", config);
  layer.down = Linear::load(weights, "down", config);
  if (bits && layer.router->bits != 8) throw std::runtime_error("mixed router bits");
  auto x = mx::astype(fixture({1, length, 64}, 0.7f), dtype);
  auto actual = layer.mlp(x, config);
  // Independent reference: sort on CPU, run each selected expert separately
  // against the dequantized tiny fixtures and accumulate its weighted output.
  auto probabilities = mx::astype(mx::softmax((*layer.router)(x), -1, true), mx::float32);
  mx::eval(probabilities);
  auto p = probabilities.data<float>();
  auto gate = unpack(layer.gate), up = unpack(layer.up), down = unpack(layer.down);
  std::vector<mx::array> rows;
  for (int token = 0; token < length; ++token) {
    std::vector<int> order{0, 1, 2};
    std::sort(order.begin(), order.end(), [&](int a, int b) {
      return p[token * 3 + a] > p[token * 3 + b];
    });
    float total = 0;
    for (int i = 0; i < k; ++i) total += p[token * 3 + order[i]];
    auto row = mx::reshape(mx::take(x, token, 1), {1, 64});
    auto sum = mx::zeros({1, 64});
    for (int i = 0; i < k; ++i) {
      int e = order[i];
      auto g = mx::matmul(row, mx::transpose(mx::take(gate, e, 0)));
      auto u = mx::matmul(row, mx::transpose(mx::take(up, e, 0)));
      auto y = mx::matmul(g * mx::sigmoid(g) * u, mx::transpose(mx::take(down, e, 0)));
      sum = sum + y * mx::array(p[token * 3 + e] / (normalize ? total : 1.0f));
    }
    rows.push_back(sum);
  }
  auto reference = mx::reshape(mx::concatenate(rows, 0), {1, length, 64});
  auto error = mx::astype(mx::max(mx::abs(actual - reference)), mx::float32);
  mx::eval(error);
  if (error.item<float>() > (dtype == mx::bfloat16 ? 2e-4f : 2e-5f)) {
    throw std::runtime_error("MoE differs from per-expert reference: " + std::to_string(error.item<float>()));
  }
}

void streamed_weights_check() {
  auto before = mx::get_active_memory();
  {
    std::unique_ptr<Weights> weights(static_cast<Weights*>(loom_mlx_weights_new()));
    int64_t shape[]{1024, 1024};
    size_t bytes = 1024 * 1024 * sizeof(float);
    char error[1024]{};
    auto data = static_cast<float*>(loom_mlx_weights_alloc(weights.get(), "streamed.weight",
        2, shape, 2, bytes, error, sizeof(error)));
    if (!data) throw std::runtime_error(error);
    auto array = take(*weights, "streamed.weight");
    if (array.data<float>() != data || mx::get_active_memory() > before + bytes + 4096) {
      throw std::runtime_error("streamed weight allocated an extra copy");
    }
    std::fill(data, data + 1024 * 1024, 1.25f);
    auto total = mx::sum(array);
    mx::eval(total);
    if (total.item<float>() != 1.25f * 1024 * 1024) {
      throw std::runtime_error("MLX did not read caller-filled storage");
    }
    auto active = mx::get_active_memory();
    if (loom_mlx_weights_alloc(weights.get(), "streamed.weight", 2, shape, 2, bytes, error, sizeof(error)) ||
        loom_mlx_weights_alloc(weights.get(), "wrong.bytes", 2, shape, 2, bytes - 1, error, sizeof(error))) {
      throw std::runtime_error("invalid streamed allocation accepted");
    }
    int64_t overflow[]{INT32_MAX, INT32_MAX, INT32_MAX};
    if (loom_mlx_weights_alloc(weights.get(), "overflow", 2, overflow, 3, bytes, error, sizeof(error)) ||
        mx::get_active_memory() > active || weights->size() != 1) {
      throw std::runtime_error("failed streamed allocation leaked or changed weights");
    }
  }
  if (mx::get_active_memory() != before) throw std::runtime_error("pending streamed weights leaked");
  mx::clear_cache();
}

void checkpoint_check(int kv_bits) {
  LoomMlxConfig config{};
  config.hidden = config.head_dim = config.kv_group = config.group_size = 64;
  config.layers = 2;
  config.heads = config.kv_heads = config.tied_embeddings = 1;
  config.eps = 1e-6f;
  config.rope_theta = 10000;
  config.bits = 3;
  config.prefill_chunk = 2;
  config.kv_bits = kv_bits;
  config.experts = 3;
  config.experts_per_token = 2;
  config.norm_topk_prob = 1;
  Weights weights;
  insert(weights, "model.embed_tokens", fixture({16, 64}, 0.3f), 3);
  weights.insert_or_assign("model.norm.weight", mx::ones({64}));
  for (int l = 0; l < config.layers; ++l) {
    std::string prefix = "model.layers." + std::to_string(l);
    weights.insert_or_assign(prefix + ".input_layernorm.weight", mx::ones({64}));
    weights.insert_or_assign(prefix + ".post_attention_layernorm.weight", mx::ones({64}));
    float phase = 0;
    for (const auto* projection : {"q_proj", "k_proj", "v_proj", "o_proj"}) {
      insert(weights, prefix + ".self_attn." + projection, fixture({64, 64}, phase += 0.6f), 3);
    }
    insert(weights, prefix + ".mlp.gate", fixture({3, 64}, 0.1f), 8);
    for (const auto* projection : {"gate_proj", "up_proj", "down_proj"}) {
      insert(weights, prefix + ".mlp.switch_mlp." + projection,
          fixture({3, 64, 64}, phase += 0.6f), 3);
    }
  }
  struct TempFile {
    char path[32] = "/tmp/loom-mlx-moe-XXXXXX";
    TempFile() {
      int fd = mkstemp(path);
      if (fd < 0) throw std::runtime_error("cannot create test checkpoint");
      close(fd);
    }
    ~TempFile() {
      std::remove(path);
      std::remove((std::string(path) + ".safetensors").c_str());
    }
  } file;
  auto saved = std::string(file.path) + ".safetensors";
  mx::save_safetensors(saved, weights);
  char error[1024]{};
  void* handle = loom_mlx_load(saved.c_str(), &config, error, sizeof(error));
  if (!handle) throw std::runtime_error(error);
  std::unique_ptr<Model, void (*)(Model*)> model(static_cast<Model*>(handle),
      [](Model* m) { loom_mlx_free(m); });
  for (const auto& layer : model->layers) {
    if (!layer.router || layer.router->bits != 8 || layer.gate.bits != 3) {
      throw std::runtime_error("checkpoint did not load mixed-precision experts");
    }
  }
  const int32_t tokens[] = {1, 2, 3};
  int32_t sampled[3]{};
  if (loom_mlx_verify(handle, tokens, 3, 0, 0, 0, sampled, error, sizeof(error))) {
    throw std::runtime_error(error);
  }
  {
    std::ifstream input(saved, std::ios::binary);
    std::vector<char> payload((std::istreambuf_iterator<char>(input)), {});
    std::ofstream output(file.path, std::ios::binary | std::ios::trunc);
    std::vector<char> padding(4096, 'x');
    output.write(padding.data(), padding.size());
    output.write(payload.data(), payload.size());
    output.write(padding.data(), padding.size());
    output.close();
    auto descriptor = "lmx:4096:" + std::to_string(payload.size()) + ":" + file.path;
    auto sliced_handle = loom_mlx_load(descriptor.c_str(), &config, error, sizeof(error));
    if (!sliced_handle) throw std::runtime_error(error);
    std::unique_ptr<Model, void (*)(Model*)> sliced(static_cast<Model*>(sliced_handle),
        [](Model* m) { loom_mlx_free(m); });
    int32_t sliced_tokens[3]{};
    if (loom_mlx_verify(sliced_handle, tokens, 3, 0, 0, 0, sliced_tokens, error, sizeof(error))) throw std::runtime_error(error);
    if (!std::equal(sampled, sampled + 3, sliced_tokens)) throw std::runtime_error("LMX slice changed greedy MoE output");
    SliceReader reader(file.path, 4096, payload.size());
    char byte;
    reader.read(&byte, 1, 0);
    if (byte != payload[0]) throw std::runtime_error("LMX slice has wrong base offset");
    bool rejected = false;
    try { reader.read(&byte, 1, payload.size()); } catch (const std::runtime_error&) { rejected = true; }
    if (!rejected) throw std::runtime_error("LMX slice read escaped payload");
    rejected = false;
    try { reader.seek(-1, std::ios_base::beg); } catch (const std::runtime_error&) { rejected = true; }
    if (!rejected) throw std::runtime_error("LMX slice seek escaped payload");
  }
  for (int count = 1; count <= 3; ++count) {
    int32_t next;
    if (loom_mlx_next(handle, tokens, count, 0, 0, 0, &next, error, sizeof(error))) {
      throw std::runtime_error(error);
    }
    if (next != sampled[count - 1]) throw std::runtime_error("verify differs from prefix recomputation");
  }
  int32_t cached_next, full_next;
  int32_t extended[] = {1, 2, 3, sampled[2]};
  if (loom_mlx_next(handle, &extended[3], 1, 3, 0, 0, &cached_next, error, sizeof(error)) ||
      loom_mlx_next(handle, extended, 4, 0, 0, 0, &full_next, error, sizeof(error))) {
    throw std::runtime_error(error);
  }
  if (cached_next != full_next) throw std::runtime_error("cached MoE decode differs from full recomputation");
  loom_mlx_reset(handle);
  if (model->offset != 0 || model->keys[0] || !model->qkeys[0].empty()) {
    throw std::runtime_error("governed reset retained KV state");
  }
  if (loom_mlx_prefill(handle, extended, 2, 0, error, sizeof(error)) ||
      loom_mlx_prefill(handle, &extended[2], 1, 2, error, sizeof(error)) ||
      loom_mlx_next(handle, &extended[3], 1, 3, 0, 0, &cached_next, error, sizeof(error))) {
    throw std::runtime_error(error);
  }
  if (cached_next != full_next) throw std::runtime_error("yielding prefill differs from full recomputation");
}
}  // namespace

int main() {
  try {
    streamed_weights_check();
    Model cache{LoomMlxConfig{}};
    cache.offset = 1000;
    cache.keys = {mx::zeros({1, 1, 1024, 64})};
    cache.values = cache.keys;
    cache.qkeys = {{mx::zeros({1, 1, 1024, 8})}};
    cache.qvalues = cache.qkeys;
    cache.rewind(32);
    if (cache.offset != 32 || !cache.keys[0] || cache.qkeys[0].empty()) {
      throw std::runtime_error("prefix rollback lost cache");
    }
    cache.rewind(0);
    if (cache.offset || cache.keys[0] || cache.values[0] ||
        !cache.qkeys[0].empty() || !cache.qvalues[0].empty()) {
      throw std::runtime_error("new conversation retained old KV capacity");
    }
    for (auto dtype : {mx::float32, mx::bfloat16}) {
      for (int bits : {0, 3, 4}) {
        for (int length : {1, 4}) {
          for (int k : {1, 2, 3}) {
            for (bool normalize : {false, true}) routing_check(length, k, normalize, bits, dtype);
          }
        }
      }
    }
    checkpoint_check(0);
    checkpoint_check(4);
    std::cout << "PASS: streamed weight ownership, 72 MoE cases, KV release/rollback, saved MoE checkpoint and bounded LMX slices (f16 and q4 KV)\n";
  } catch (const std::exception& e) {
    std::cerr << e.what() << '\n';
    return 1;
  }
}
