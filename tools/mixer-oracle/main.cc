// WebRTC mixer/limiter reference-vector generator.
//
// Drives a minimal extraction of WebRTC's mono FrameCombiner semantics and the
// real AGC2 Limiter from /tmp/webrtc. Output files are committed under
// tests/mixer_vectors and normal Rust tests read those files only.

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <string>
#include <vector>

#include "api/audio/audio_view.h"
#include "common_audio/include/audio_util.h"
#include "modules/audio_processing/agc2/limiter.h"
#include "modules/audio_processing/logging/apm_data_dumper.h"

namespace {

constexpr size_t kFrameSamples = 480;

std::string g_out_dir = "vectors";

struct Xorshift {
  uint32_t s;
  explicit Xorshift(uint32_t seed) : s(seed ? seed : 0x12345678u) {}

  uint32_t next() {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    return s;
  }

  int centered(int amplitude) {
    return static_cast<int>(next() % (2u * amplitude + 1u)) - amplitude;
  }
};

template <typename T>
void Dump(const std::string& name,
          const std::vector<std::string>& meta,
          const std::vector<T>& values) {
  std::filesystem::create_directories(g_out_dir);
  std::string path = g_out_dir + "/" + name + ".txt";
  FILE* f = std::fopen(path.c_str(), "w");
  if (!f) {
    std::fprintf(stderr, "cannot open %s\n", path.c_str());
    std::exit(1);
  }
  std::fprintf(f, "# %s\n", name.c_str());
  for (const auto& m : meta) {
    std::fprintf(f, "# %s\n", m.c_str());
  }
  for (const auto& v : values) {
    std::fprintf(f, "%lld\n", static_cast<long long>(v));
  }
  std::fclose(f);
  std::printf("wrote %s (%zu values)\n", path.c_str(), values.size());
}

std::vector<int16_t> ToneNoise(size_t n,
                               int sine_amp,
                               double period,
                               int noise_amp,
                               uint32_t seed) {
  Xorshift rng(seed);
  std::vector<int16_t> out(n);
  for (size_t i = 0; i < n; ++i) {
    const double phase = 2.0 * 3.14159265358979323846 *
                         static_cast<double>(i) / period;
    const double tone = sine_amp * std::sin(phase);
    const int sample =
        static_cast<int>(tone < 0 ? tone - 0.5 : tone + 0.5) +
        rng.centered(noise_amp);
    out[i] = static_cast<int16_t>(
        std::clamp(sample, static_cast<int>(INT16_MIN),
                   static_cast<int>(INT16_MAX)));
  }
  return out;
}

std::vector<int16_t> LimiterInputSequence() {
  Xorshift rng(0xC0FFEEu);
  std::vector<int16_t> out(4 * kFrameSamples);
  for (size_t i = 0; i < out.size(); ++i) {
    const size_t frame = i / kFrameSamples;
    const double phase = 2.0 * 3.14159265358979323846 *
                         static_cast<double>(i) / 43.0;
    int amp = 0;
    switch (frame) {
      case 0:
        amp = 31500;
        break;
      case 1:
        amp = 28000;
        break;
      case 2:
        amp = 12000;
        break;
      default:
        amp = 30000;
        break;
    }
    const double tone = amp * std::sin(phase);
    const int sample =
        static_cast<int>(tone < 0 ? tone - 0.5 : tone + 0.5) +
        rng.centered(900);
    out[i] = static_cast<int16_t>(
        std::clamp(sample, static_cast<int>(INT16_MIN),
                   static_cast<int>(INT16_MAX)));
  }
  return out;
}

std::vector<int16_t> CombineMono(const std::vector<std::vector<int16_t>>& frames,
                                 size_t number_of_streams,
                                 bool use_limiter,
                                 webrtc::Limiter* limiter) {
  std::vector<int16_t> out(kFrameSamples, 0);

  // Mirrors WebRTC frame_combiner.cc's MixFewFramesWithNoLimiter(): with zero
  // or one registered stream, muted frames are absent and one normal frame is
  // copied without limiter processing.
  if (number_of_streams <= 1) {
    if (!frames.empty()) {
      out = frames[0];
    }
    return out;
  }

  std::array<float, kFrameSamples> mix = {};
  for (const auto& frame : frames) {
    for (size_t i = 0; i < kFrameSamples; ++i) {
      mix[i] += static_cast<float>(frame[i]);
    }
  }

  if (use_limiter) {
    webrtc::DeinterleavedView<float> view(mix.data(), kFrameSamples, 1);
    limiter->Process(view);
  }

  for (size_t i = 0; i < kFrameSamples; ++i) {
    out[i] = webrtc::FloatS16ToS16(mix[i]);
  }
  return out;
}

std::vector<int16_t> ProcessLimiterSequence(
    const std::vector<int16_t>& input) {
  webrtc::ApmDataDumper dumper(0);
  webrtc::Limiter limiter(&dumper, kFrameSamples, "ChattMixerOracle");
  std::vector<int16_t> out;
  out.reserve(input.size());

  for (size_t offset = 0; offset < input.size(); offset += kFrameSamples) {
    std::array<float, kFrameSamples> frame = {};
    for (size_t i = 0; i < kFrameSamples; ++i) {
      frame[i] = static_cast<float>(input[offset + i]);
    }
    webrtc::DeinterleavedView<float> view(frame.data(), kFrameSamples, 1);
    limiter.Process(view);
    for (float sample : frame) {
      out.push_back(webrtc::FloatS16ToS16(sample));
    }
  }

  return out;
}

void GenerateVectors() {
  const auto input_a = ToneNoise(kFrameSamples, 9000, 37.0, 1600, 0xA11CEu);
  const auto input_b = ToneNoise(kFrameSamples, 7600, 29.0, 1400, 0xB0Bu);
  const auto hot_a = ToneNoise(kFrameSamples, 26000, 53.0, 900, 0xFACEu);
  const auto hot_b = ToneNoise(kFrameSamples, 24000, 47.0, 1100, 0xBEEFu);
  const auto limiter_input = LimiterInputSequence();

  Dump("combiner_input_a", {"int16 normal frame A"}, input_a);
  Dump("combiner_input_b", {"int16 normal frame B"}, input_b);
  Dump("combiner_input_hot_a", {"int16 hot frame A"}, hot_a);
  Dump("combiner_input_hot_b", {"int16 hot frame B"}, hot_b);
  Dump("limiter_sequence_input", {"four 480-sample FloatS16 frames as ints"},
       limiter_input);

  webrtc::ApmDataDumper zero_dumper(0);
  webrtc::Limiter zero_limiter(&zero_dumper, kFrameSamples,
                               "ChattMixerOracle.Zero");
  Dump("combiner_zero_no_limiter", {"number_of_streams=0", "use_limiter=0"},
       CombineMono({}, 0, false, &zero_limiter));

  webrtc::ApmDataDumper one_dumper(0);
  webrtc::Limiter one_limiter(&one_dumper, kFrameSamples,
                              "ChattMixerOracle.One");
  Dump("combiner_one_no_limiter",
       {"normal_frames=input_a", "number_of_streams=1", "use_limiter=0"},
       CombineMono({input_a}, 1, false, &one_limiter));

  webrtc::ApmDataDumper two_dumper(0);
  webrtc::Limiter two_limiter(&two_dumper, kFrameSamples,
                              "ChattMixerOracle.TwoNoLimiter");
  Dump("combiner_two_no_limiter",
       {"normal_frames=input_a,input_b", "number_of_streams=2",
        "use_limiter=0"},
       CombineMono({input_a, input_b}, 2, false, &two_limiter));

  webrtc::ApmDataDumper one_multi_dumper(0);
  webrtc::Limiter one_multi_limiter(&one_multi_dumper, kFrameSamples,
                                    "ChattMixerOracle.OneMultiLimiter");
  Dump("combiner_one_normal_two_streams_limiter",
       {"normal_frames=input_a", "number_of_streams=2", "use_limiter=1"},
       CombineMono({input_a}, 2, true, &one_multi_limiter));

  webrtc::ApmDataDumper hot_dumper(0);
  webrtc::Limiter hot_limiter(&hot_dumper, kFrameSamples,
                              "ChattMixerOracle.TwoLimiter");
  Dump("combiner_two_with_limiter",
       {"normal_frames=hot_a,hot_b", "number_of_streams=2", "use_limiter=1"},
       CombineMono({hot_a, hot_b}, 2, true, &hot_limiter));

  Dump("limiter_sequence_quantized",
       {"real WebRTC Limiter output, FloatS16ToS16 quantized",
        "input=limiter_sequence_input"},
       ProcessLimiterSequence(limiter_input));
}

}  // namespace

int main(int argc, char** argv) {
  if (argc > 1) {
    g_out_dir = argv[1];
  }
  GenerateVectors();
  return 0;
}
