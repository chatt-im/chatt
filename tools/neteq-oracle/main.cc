// NetEQ DSP reference-vector generator.
//
// Drives the real WebRTC signal_processing and neteq DSP routines with fixed,
// deterministic inputs and writes their exact integer outputs to text files.
// The Rust port asserts byte-for-byte equality against these files.
//
// Output format (one file per case): lines starting with '#' are comments
// (name + metadata as key=value); every other non-empty line is one decimal
// integer. Order is significant.
//
// Determinism: inputs come from a fixed xorshift generator and closed-form
// waveforms only. Nothing reads wall-clock time or the system RNG.

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "common_audio/signal_processing/include/signal_processing_library.h"
#include "common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.h"
#include "modules/audio_coding/neteq/accelerate.h"
#include "modules/audio_coding/neteq/audio_multi_vector.h"
#include "modules/audio_coding/neteq/background_noise.h"
#include "api/neteq/neteq.h"
#include "api/neteq/tick_timer.h"
#include "modules/audio_coding/neteq/cross_correlation.h"
#include "modules/audio_coding/neteq/normal.h"
#include "modules/audio_coding/neteq/preemptive_expand.h"
#include "modules/audio_coding/neteq/dsp_helper.h"
#include "modules/audio_coding/neteq/expand.h"
#include "modules/audio_coding/neteq/merge.h"
#include "modules/audio_coding/neteq/random_vector.h"
#include "modules/audio_coding/neteq/statistics_calculator.h"
#include "modules/audio_coding/neteq/sync_buffer.h"

namespace {

std::string g_out_dir = "vectors";

// Deterministic int16 pseudo-noise. Fixed seed, xorshift32, mapped to a
// configurable amplitude. Independent of any platform RNG.
struct Xorshift {
  uint32_t s;
  explicit Xorshift(uint32_t seed) : s(seed ? seed : 0x12345678u) {}
  uint32_t next() {
    s ^= s << 13;
    s ^= s >> 17;
    s ^= s << 5;
    return s;
  }
  int16_t sample(int amplitude) {
    int32_t v = static_cast<int32_t>(next() % (2u * amplitude + 1u)) - amplitude;
    return static_cast<int16_t>(v);
  }
};

std::vector<int16_t> Noise(size_t n, int amplitude, uint32_t seed) {
  Xorshift rng(seed);
  std::vector<int16_t> v(n);
  for (size_t i = 0; i < n; ++i) v[i] = rng.sample(amplitude);
  return v;
}

// Fixed-point sine via integer phase. amplitude in int16 units.
std::vector<int16_t> Sine(size_t n, double period, int amplitude) {
  std::vector<int16_t> v(n);
  for (size_t i = 0; i < n; ++i) {
    double phase = 2.0 * 3.14159265358979323846 * static_cast<double>(i) / period;
    // Round-half-away-from-zero to a stable integer independent of libm rint mode.
    double s = amplitude * __builtin_sin(phase);
    v[i] = static_cast<int16_t>(s < 0 ? s - 0.5 : s + 0.5);
  }
  return v;
}

// Reads a vector file written by Dump (or hand-authored): one decimal integer
// per non-comment line. Used by CasePipeline to read the real-audio input that
// the Rust test also reads, so both sides drive identical samples.
std::vector<int16_t> LoadI16(const std::string& name) {
  std::string path = g_out_dir + "/" + name + ".txt";
  FILE* f = std::fopen(path.c_str(), "r");
  if (!f) {
    std::fprintf(stderr, "cannot open %s for reading\n", path.c_str());
    std::exit(1);
  }
  std::vector<int16_t> out;
  // getline reads whole lines regardless of length, so long comment lines are
  // never split into spurious numeric fragments (a 64-byte fgets buffer was).
  char* line = nullptr;
  size_t cap = 0;
  while (getline(&line, &cap, f) != -1) {
    if (line[0] == '#' || line[0] == '\n' || line[0] == '\0') continue;
    out.push_back(static_cast<int16_t>(std::atoi(line)));
  }
  std::free(line);
  std::fclose(f);
  return out;
}

template <typename T>
void Dump(const std::string& name,
          const std::vector<std::string>& meta,
          const std::vector<T>& values) {
  std::string path = g_out_dir + "/" + name + ".txt";
  FILE* f = std::fopen(path.c_str(), "w");
  if (!f) {
    std::fprintf(stderr, "cannot open %s\n", path.c_str());
    std::exit(1);
  }
  std::fprintf(f, "# %s\n", name.c_str());
  for (const auto& m : meta) std::fprintf(f, "# %s\n", m.c_str());
  for (const auto& v : values) {
    std::fprintf(f, "%lld\n", static_cast<long long>(v));
  }
  std::fclose(f);
  std::printf("wrote %s (%zu values)\n", path.c_str(), values.size());
}

// ---- component cases -------------------------------------------------------

void CaseCrossCorrelation() {
  auto a = Noise(256, 8000, 0xA1);
  auto b = Noise(256, 8000, 0xB2);
  Dump("spl_cross_correlation_in_a", {"Noise(256,8000,0xA1)"}, a);
  Dump("spl_cross_correlation_in_b", {"Noise(256,8000,0xB2)"}, b);
  const size_t dim_seq = 160;
  const size_t dim_cc = 40;
  for (int shift : {0, 3, 6}) {
    std::vector<int32_t> cc(dim_cc);
    WebRtcSpl_CrossCorrelation(cc.data(), a.data(), b.data(), dim_seq, dim_cc,
                               shift, 1);
    Dump("spl_cross_correlation_shift" + std::to_string(shift),
         {"dim_seq=160", "dim_cc=40", "step=1", "shift=" + std::to_string(shift)},
         cc);
  }
}

void CaseCrossCorrelationAutoShift() {
  // Mirror time_stretch::AutoCorrelation: seq1 and seq2 point into one buffer,
  // seq2 earlier than seq1, step -1 (so seq2 is read backwards but in bounds).
  auto buf = Noise(256, 12000, 0xC3);
  Dump("spl_cross_correlation_auto_shift_in", {"Noise(256,12000,0xC3)"}, buf);
  const size_t s1_off = 100;
  const size_t s1_len = 50;
  const size_t s2_off = 60;  // s1_off - (max_lag - min_lag)
  const size_t cc_len = 40;
  std::vector<int32_t> cc(cc_len);
  int scaling = webrtc::CrossCorrelationWithAutoShift(
      buf.data() + s1_off, buf.data() + s2_off, s1_len, cc_len, -1, cc.data());
  std::vector<int32_t> out;
  out.push_back(scaling);
  for (int32_t v : cc) out.push_back(v);
  Dump("spl_cross_correlation_auto_shift",
       {"buf=Noise(256,12000,0xC3)", "s1_off=100", "s1_len=50", "s2_off=60",
        "cc_len=40", "step=-1", "first_value_is_scaling"},
       out);
}

void CaseAutoCorrelation() {
  auto a = Sine(256, 37.0, 9000);
  Dump("spl_auto_correlation_in", {"Sine(256,37,9000)"}, a);
  const size_t order = 8;
  std::vector<int32_t> result(order + 1);
  int scale = 0;
  WebRtcSpl_AutoCorrelation(a.data(), a.size(), order, result.data(), &scale);
  std::vector<int32_t> out;
  out.push_back(scale);
  for (int32_t v : result) out.push_back(v);
  Dump("spl_auto_correlation",
       {"len=256", "order=8", "first_value_is_scale"}, out);
}

void CaseLevinsonDurbin() {
  // Build an autocorrelation from a sine so the system is well conditioned.
  auto a = Sine(256, 41.0, 10000);
  const size_t order = 6;
  std::vector<int32_t> R(order + 1);
  int scale = 0;
  WebRtcSpl_AutoCorrelation(a.data(), a.size(), order, R.data(), &scale);
  Dump("spl_levinson_durbin_in", {"autocorr R[7] (int32)"}, R);
  std::vector<int16_t> lpc(order + 1);
  std::vector<int16_t> refl(order);
  int16_t stable = WebRtcSpl_LevinsonDurbin(R.data(), lpc.data(), refl.data(),
                                            order);
  std::vector<int32_t> out;
  out.push_back(stable);
  for (int16_t v : lpc) out.push_back(v);
  for (int16_t v : refl) out.push_back(v);
  Dump("spl_levinson_durbin",
       {"order=6", "layout=stable,lpc[7],refl[6]"}, out);
}

void CaseFilterArFastQ12() {
  const size_t len = 160;
  auto in = Noise(len, 6000, 0xE5);
  Dump("spl_filter_ar_fast_q12_in", {"Noise(160,6000,0xE5)"}, in);
  // A modest AR filter in Q12, coefs[0] == 4096 (1.0).
  std::vector<int16_t> coefs = {4096, -2048, 1024, -512, 256, -128, 64};
  const size_t order = coefs.size() - 1;
  // FilterARFastQ12 reads data_out at negative indices (filter state). Provide
  // `order` leading state samples and point data_out past them.
  std::vector<int16_t> scratch(order + len, 0);
  for (size_t i = 0; i < order; ++i) scratch[i] = static_cast<int16_t>(100 * (int)i - 250);
  WebRtcSpl_FilterARFastQ12(in.data(), scratch.data() + order, coefs.data(),
                            coefs.size(), len);
  std::vector<int16_t> out(scratch.begin() + order, scratch.end());
  Dump("spl_filter_ar_fast_q12",
       {"len=160", "order=6", "state=100*i-250"}, out);
}

void CaseSqrtFloor() {
  std::vector<int32_t> inputs = {0,     1,      2,      3,       4,
                                 100,   65535,  65536,  1000000, 16777216,
                                 2000000000};
  std::vector<int32_t> out;
  for (int32_t v : inputs) out.push_back(WebRtcSpl_SqrtFloor(v));
  Dump("spl_sqrt_floor",
       {"inputs=0,1,2,3,4,100,65535,65536,1000000,16777216,2000000000"}, out);
}

void CaseDivW32W16() {
  struct Pair {
    int32_t num;
    int16_t den;
  };
  std::vector<Pair> pairs = {{1000000, 7},   {-1000000, 7}, {123456789, 1234},
                             {2147483647, 2}, {5, 32767},    {-5, 3}};
  std::vector<int32_t> out;
  for (auto p : pairs) out.push_back(WebRtcSpl_DivW32W16(p.num, p.den));
  Dump("spl_div_w32w16", {"pairs=(1e6,7)(-1e6,7)(123456789,1234)(maxi,2)(5,32767)(-5,3)"},
       out);
}

void CaseRandomVector() {
  webrtc::RandomVector rv;
  rv.Reset();
  std::vector<int16_t> a(120);
  rv.Generate(a.size(), a.data());
  rv.IncreaseSeedIncrement(2);
  std::vector<int16_t> b(120);
  rv.Generate(b.size(), b.data());
  std::vector<int16_t> out;
  out.insert(out.end(), a.begin(), a.end());
  out.insert(out.end(), b.begin(), b.end());
  Dump("random_vector_generate",
       {"reset,gen120,increment+2,gen120", "two_blocks_of_120"}, out);
}

void CaseDownsampleTo4kHz() {
  // 110 output samples at /12 plus filter delay needs a long input window.
  auto in = Sine(1920, 53.0, 12000);
  Dump("dsp_downsample_to_4khz_in", {"Sine(1920,53,12000)"}, in);
  std::vector<int16_t> out(110);
  int r = webrtc::DspHelper::DownsampleTo4kHz(in.data(), in.size(), out.size(),
                                              48000, true, out.data());
  std::vector<int32_t> v;
  v.push_back(r);
  for (int16_t s : out) v.push_back(s);
  Dump("dsp_downsample_to_4khz",
       {"in_len=960", "out_len=110", "rate=48000", "compensate=true",
        "first_value_is_return"},
       v);
}

void CasePeakDetection() {
  // 50-sample correlation curve with a couple of humps.
  std::vector<int16_t> data(50);
  for (size_t i = 0; i < data.size(); ++i) {
    double x = static_cast<double>(i);
    double y = 8000.0 * __builtin_sin(x * 0.5) + 4000.0 * __builtin_sin(x * 0.17);
    data[i] = static_cast<int16_t>(y < 0 ? y - 0.5 : y + 0.5);
  }
  Dump("dsp_peak_detection_in", {"len=50 dual-hump curve"}, data);
  const size_t num_peaks = 2;
  const int fs_mult = 6;
  std::vector<size_t> peak_index(num_peaks);
  std::vector<int16_t> peak_value(num_peaks);
  // PeakDetection may read one past data_length for num_peaks==1; give slack.
  data.push_back(0);
  webrtc::DspHelper::PeakDetection(data.data(), 50, num_peaks, fs_mult,
                                   peak_index.data(), peak_value.data());
  std::vector<int32_t> out;
  for (size_t p : peak_index) out.push_back(static_cast<int32_t>(p));
  for (int16_t v : peak_value) out.push_back(v);
  Dump("dsp_peak_detection",
       {"len=50", "num_peaks=2", "fs_mult=6", "layout=index[2],value[2]"}, out);
}

void CaseParabolicFit() {
  std::vector<std::vector<int16_t>> triples = {
      {1000, 2000, 1500}, {2000, 2000, 2000}, {500, 9000, 8000}, {-100, 50, -200}};
  const int fs_mult = 6;
  std::vector<int32_t> out;
  for (auto t : triples) {
    size_t idx = 10;  // Realistic peak index (PeakDetection never passes 0).
    int16_t val = 0;
    webrtc::DspHelper::ParabolicFit(t.data(), fs_mult, &idx, &val);
    out.push_back(static_cast<int32_t>(idx));
    out.push_back(val);
  }
  Dump("dsp_parabolic_fit", {"fs_mult=6", "layout=(index,value)_per_triple"},
       out);
}

void CaseCrossFade() {
  auto in1 = Sine(80, 23.0, 10000);
  auto in2 = Sine(80, 31.0, 8000);
  Dump("dsp_cross_fade_in1", {"Sine(80,23,10000)"}, in1);
  Dump("dsp_cross_fade_in2", {"Sine(80,31,8000)"}, in2);
  int16_t mix_factor = 16384;
  const int16_t decrement = 16384 / 80;
  std::vector<int16_t> out(80);
  webrtc::DspHelper::CrossFade(in1.data(), in2.data(), 80, &mix_factor,
                               decrement, out.data());
  Dump("dsp_cross_fade",
       {"len=80", "mix_start=16384", "decrement=204"}, out);
}

// ---- operation cases -------------------------------------------------------

void CaseBackgroundNoise() {
  // Low-amplitude white noise: spectrally flat and low energy, so Update
  // initializes the model (stable LPC + flat-spectrum + below threshold).
  const size_t sig_len = 320;
  auto signal = Noise(sig_len, 800, 0x5151);
  Dump("background_noise_in", {"Noise(320,800,0x5151)"}, signal);

  webrtc::AudioMultiVector sync(1);
  sync.PushBackInterleaved(signal);

  webrtc::BackgroundNoise bg(1);
  bool updated = bg.Update(sync);

  std::vector<int32_t> params;
  params.push_back(updated ? 1 : 0);
  params.push_back(bg.initialized() ? 1 : 0);
  params.push_back(bg.Energy(0));
  params.push_back(bg.MuteFactor(0));
  params.push_back(bg.Scale(0));
  params.push_back(bg.ScaleShift(0));
  for (size_t i = 0; i < webrtc::BackgroundNoise::kMaxLpcOrder + 1; ++i)
    params.push_back(bg.Filter(0)[i]);
  for (size_t i = 0; i < webrtc::BackgroundNoise::kMaxLpcOrder; ++i)
    params.push_back(bg.FilterState(0)[i]);
  Dump("background_noise_params",
       {"layout=updated,initialized,energy,mute_factor,scale,scale_shift,"
        "filter[9],filter_state[8]"},
       params);

  // Generate one noise block from a deterministic random vector.
  const size_t kOrder = webrtc::BackgroundNoise::kMaxLpcOrder;
  const size_t num_noise = 80;
  webrtc::RandomVector rv;
  std::vector<int16_t> random_vector(num_noise);
  rv.Generate(num_noise, random_vector.data());
  std::vector<int16_t> buffer(kOrder + num_noise, 0);
  bg.GenerateBackgroundNoise(random_vector, 0, 0, false, num_noise,
                             buffer.data());
  std::vector<int16_t> noise(buffer.begin() + kOrder, buffer.end());
  Dump("background_noise_generate",
       {"num_noise=80", "random=RandomVector default gen80"}, noise);
}

void CaseAccelerate() {
  // 30 ms periodic input (200 Hz, period 240) so VAD reports active speech and
  // the pitch peak is strong.
  auto input = Sine(1440, 240.0, 8000);
  Dump("accelerate_in", {"Sine(1440,240,8000)"}, input);

  for (bool fast : {false, true}) {
    webrtc::BackgroundNoise bg(1);
    webrtc::Accelerate acc(48000, 1, bg);
    webrtc::AudioMultiVector output(1);
    size_t length_change = 0;
    auto rc = acc.Process(input.data(), input.size(), fast, &output,
                          &length_change);

    std::vector<int32_t> out;
    out.push_back(static_cast<int32_t>(rc));
    out.push_back(static_cast<int32_t>(length_change));
    out.push_back(static_cast<int32_t>(output.Size()));
    for (size_t i = 0; i < output.Size(); ++i) out.push_back(output[0][i]);
    Dump(fast ? "accelerate_fast" : "accelerate_normal",
         {"layout=return_code,length_change,size,samples"}, out);
  }
}

void CasePreemptiveExpand() {
  auto input = Sine(1440, 240.0, 8000);
  Dump("preemptive_expand_in", {"Sine(1440,240,8000)"}, input);

  const size_t overlap_samples = 240;
  const size_t old_data_length = 480;
  webrtc::BackgroundNoise bg(1);
  webrtc::PreemptiveExpand pe(48000, 1, bg, overlap_samples);
  webrtc::AudioMultiVector output(1);
  size_t length_change = 0;
  auto rc = pe.Process(input.data(), input.size(), old_data_length, &output,
                       &length_change);

  std::vector<int32_t> out;
  out.push_back(static_cast<int32_t>(rc));
  out.push_back(static_cast<int32_t>(length_change));
  out.push_back(static_cast<int32_t>(output.Size()));
  for (size_t i = 0; i < output.Size(); ++i) out.push_back(output[0][i]);
  Dump("preemptive_expand_normal",
       {"overlap=240", "old_data=480",
        "layout=return_code,length_change,size,samples"},
       out);
}

void CaseExpand() {
  const size_t signal_length = 256 * 6;  // 1536 = 256 * fs_mult at 48 kHz.
  auto signal = Sine(signal_length, 240.0, 8000);
  Dump("expand_in", {"Sine(1536,240,8000)"}, signal);

  webrtc::SyncBuffer sync(1, signal_length);
  sync.Channel(0).OverwriteAt(signal.data(), signal_length, 0);

  webrtc::BackgroundNoise bg(1);
  webrtc::RandomVector rv;
  webrtc::TickTimer tick;
  webrtc::StatisticsCalculator stats(&tick);
  webrtc::Expand expand(&bg, &sync, &rv, &stats, 48000, 1);

  // Several consecutive expands exercise AnalyzeSignal (first call) and the
  // subsequent-expand path (lag cycling, progressive muting, random vector).
  for (int call = 0; call < 4; ++call) {
    webrtc::AudioMultiVector output(1);
    expand.Process(&output);
    std::vector<int32_t> out;
    out.push_back(static_cast<int32_t>(output.Size()));
    for (size_t i = 0; i < output.Size(); ++i) out.push_back(output[0][i]);
    Dump("expand_out_" + std::to_string(call), {"layout=size,samples"}, out);
  }

  // The overlap-add mutates the sync buffer tail; capture the whole buffer.
  std::vector<int16_t> tail(signal_length);
  sync.Channel(0).CopyTo(signal_length, 0, tail.data());
  Dump("expand_sync_tail", {"sync buffer after 4 expands"}, tail);
}

void CaseNormal() {
  const size_t signal_length = 256 * 6;
  auto signal = Sine(signal_length, 240.0, 8000);
  Dump("normal_sync_in", {"Sine(1536,240,8000)"}, signal);

  webrtc::SyncBuffer sync(1, signal_length);
  sync.Channel(0).OverwriteAt(signal.data(), signal_length, 0);
  webrtc::BackgroundNoise bg(1);
  webrtc::RandomVector rv;
  webrtc::TickTimer tick;
  webrtc::StatisticsCalculator stats(&tick);
  webrtc::Expand expand(&bg, &sync, &rv, &stats, 48000, 1);

  // A few concealment expands establish a non-trivial mute state.
  for (int i = 0; i < 3; ++i) {
    webrtc::AudioMultiVector o(1);
    expand.Process(&o);
  }

  auto decoded = Sine(480, 240.0, 6000);
  Dump("normal_in", {"Sine(480,240,6000)"}, decoded);

  webrtc::Normal normal(48000, nullptr, bg, &expand, &stats);
  webrtc::AudioMultiVector output(1);
  normal.Process(decoded.data(), decoded.size(), webrtc::NetEq::Mode::kExpand,
                 &output);
  std::vector<int32_t> out;
  out.push_back(static_cast<int32_t>(output.Size()));
  for (size_t i = 0; i < output.Size(); ++i) out.push_back(output[0][i]);
  Dump("normal_out", {"layout=size,samples", "last_mode=kExpand"}, out);
}

void CaseMerge() {
  const size_t signal_length = 256 * 6;  // 1536
  const size_t old_length = 60;          // FutureLength borrowed from sync.
  const size_t sync_size = signal_length + old_length;
  auto signal = Sine(sync_size, 240.0, 8000);
  Dump("merge_sync_in", {"Sine(1596,240,8000)", "old_length=60"}, signal);

  webrtc::SyncBuffer sync(1, sync_size);
  sync.Channel(0).OverwriteAt(signal.data(), sync_size, 0);
  sync.set_next_index(signal_length);  // FutureLength = old_length.

  webrtc::BackgroundNoise bg(1);
  webrtc::RandomVector rv;
  webrtc::TickTimer tick;
  webrtc::StatisticsCalculator stats(&tick);
  webrtc::Expand expand(&bg, &sync, &rv, &stats, 48000, 1);
  // Prime expand with one concealment run (Merge follows an Expand).
  {
    webrtc::AudioMultiVector o(1);
    expand.Process(&o);
  }

  auto decoded = Sine(960, 240.0, 6000);
  Dump("merge_in", {"Sine(960,240,6000)"}, decoded);

  webrtc::Merge merge(48000, 1, &expand, &sync);
  webrtc::AudioMultiVector output(1);
  size_t added = merge.Process(decoded.data(), decoded.size(), &output);

  std::vector<int32_t> out;
  out.push_back(static_cast<int32_t>(added));
  out.push_back(static_cast<int32_t>(output.Size()));
  for (size_t i = 0; i < output.Size(); ++i) out.push_back(output[0][i]);
  Dump("merge_out", {"layout=added,size,samples"}, out);

  std::vector<int16_t> after(sync.Size());
  sync.Channel(0).CopyTo(sync.Size(), 0, after.data());
  Dump("merge_sync_after", {"sync buffer after merge"}, after);
}

// ---- whole-pipeline case --------------------------------------------------
//
// Drives a long, fixed operation tape over REAL decoded speech
// (pipeline_audio_in.txt) through the actual WebRTC Expand/Merge/Normal/
// Accelerate/PreemptiveExpand classes, sharing one BackgroundNoise and one
// RandomVector exactly as NetEqImpl does. Each "tick" runs one operation, then
// extracts one 10 ms block and reinstates the overlap lookahead, mirroring the
// body of `NetEqCore::get_audio` and its `do_*` handlers in the Rust port. The
// concatenation of every extracted block (plus the final sync buffer) is the
// reference the Rust `pipeline_matches_oracle` test asserts byte-for-byte.
//
// This is a differential test of the DSP layer + sync buffer + shared
// RNG/background-noise interplay over realistic input. The operation tape is
// fixed (not NetEQ's decision logic, which the port intentionally does not
// mirror), so both languages execute the identical sequence.

namespace pipeline {

constexpr int kFs = 48000;
constexpr size_t kSync = 5760 + 60 * 48;  // 8640, SYNC_BUFFER_SAMPLES.
constexpr size_t kOut = 480;              // 10 ms.
constexpr size_t kOverlap = 30;           // Expand overlap lookahead.
constexpr size_t kReq = 1440;             // 30 ms time-scale window.

enum Op { N, E, M, A, FA, P };

// Mirrors `Mode` insofar as the harness branches on it.
enum LastMode {
  LM_NORMAL,
  LM_EXPAND,
  LM_MERGE,
  LM_ACC_SUCCESS,
  LM_ACC_LOW,
  LM_ACC_FAIL,
  LM_PE_SUCCESS,
  LM_PE_LOW,
  LM_PE_FAIL,
};

void PushZeros(webrtc::SyncBuffer& sync, size_t count) {
  if (count == 0) return;
  webrtc::AudioMultiVector z(1);
  std::vector<int16_t> zz(count, 0);
  z.PushBackInterleaved(zz);
  sync.PushBack(z);
}

void PushSamples(webrtc::SyncBuffer& sync, const std::vector<int16_t>& s) {
  webrtc::AudioMultiVector v(1);
  v.PushBackInterleaved(s);
  sync.PushBack(v);
}

}  // namespace pipeline

void CasePipeline() {
  using namespace pipeline;
  std::vector<int16_t> audio = LoadI16("pipeline_audio_in");

  webrtc::SyncBuffer sync(1, kSync);
  sync.Channel(0).OverwriteAt(audio.data(), kSync, 0);  // next_index_ defaults to kSync.

  webrtc::BackgroundNoise bg(1);
  webrtc::RandomVector rv;
  webrtc::TickTimer tick;
  webrtc::StatisticsCalculator stats(&tick);
  webrtc::Expand expand(&bg, &sync, &rv, &stats, kFs, 1);
  webrtc::Normal normal(kFs, nullptr, bg, &expand, &stats);
  webrtc::Merge merge(kFs, 1, &expand, &sync);
  webrtc::Accelerate accel(kFs, 1, bg);
  webrtc::PreemptiveExpand preempt(kFs, 1, bg, kOverlap);

  size_t cursor = kSync;
  int last_mode = LM_NORMAL;
  std::vector<int16_t> out_stream;

  auto next_frame = [&](size_t n) {
    std::vector<int16_t> f(audio.begin() + cursor, audio.begin() + cursor + n);
    cursor += n;
    return f;
  };

  // Build the operation tape. One cycle decodes kCycleDecode samples (40 Normal
  // x 480 + 3 Merge x 480 + 3 time-stretch x 1440); repeat it as many whole
  // cycles as the audio allows so the entire clip flows through the DSP.
  constexpr size_t kCycleDecode = 24960;
  size_t num_cycles = (audio.size() - kSync) / kCycleDecode;
  std::vector<Op> tape;
  auto repeat = [&](Op op, int times) {
    for (int i = 0; i < times; ++i) tape.push_back(op);
  };
  for (size_t c = 0; c < num_cycles; ++c) {
    repeat(N, 8);
    repeat(E, 2);
    tape.push_back(M);
    repeat(N, 4);
    tape.push_back(A);
    repeat(N, 3);
    tape.push_back(FA);
    repeat(N, 3);
    tape.push_back(P);
    repeat(N, 4);
    repeat(E, 6);
    tape.push_back(M);
    repeat(N, 8);
    repeat(E, 3);
    tape.push_back(M);
    repeat(N, 10);
  }

  for (Op op : tape) {
    switch (op) {
      case N: {
        auto decoded = next_frame(kOut);
        if (last_mode == LM_EXPAND) {
          webrtc::AudioMultiVector o(1);
          normal.Process(decoded.data(), decoded.size(),
                         webrtc::NetEq::Mode::kExpand, &o);
          sync.PushBack(o);
        } else {
          PushSamples(sync, decoded);
        }
        last_mode = LM_NORMAL;
        break;
      }
      case M: {
        auto decoded = next_frame(kOut);
        std::vector<int16_t> in = decoded;
        webrtc::AudioMultiVector o(1);
        merge.Process(in.data(), in.size(), &o);
        sync.PushBack(o);
        expand.Reset();
        last_mode = LM_MERGE;
        break;
      }
      case E: {
        if (expand.Muted() && !bg.initialized()) {
          size_t target = kOut + kOverlap;
          size_t future = sync.FutureLength();
          if (future < target) PushZeros(sync, target - future);
          last_mode = LM_EXPAND;
          break;
        }
        int guard = 0;
        while ((sync.FutureLength() > kOverlap ? sync.FutureLength() - kOverlap
                                               : 0) < kOut) {
          webrtc::AudioMultiVector o(1);
          expand.Process(&o);
          if (o.Size() == 0) {
            PushZeros(sync, kOut);
          } else {
            sync.PushBack(o);
          }
          last_mode = LM_EXPAND;
          if (++guard > 64) break;
        }
        last_mode = LM_EXPAND;
        break;
      }
      case A:
      case FA: {
        auto decoded = next_frame(kReq);
        bool fast = (op == FA);
        webrtc::AudioMultiVector o(1);
        size_t removed = 0;
        auto rc = accel.Process(decoded.data(), decoded.size(), fast, &o,
                                &removed);
        sync.PushBack(o);
        last_mode = rc == webrtc::Accelerate::kSuccess
                        ? LM_ACC_SUCCESS
                        : (rc == webrtc::Accelerate::kSuccessLowEnergy
                               ? LM_ACC_LOW
                               : LM_ACC_FAIL);
        expand.Reset();
        break;
      }
      case P: {
        auto decoded = next_frame(kReq);
        webrtc::AudioMultiVector o(1);
        size_t added = 0;
        auto rc = preempt.Process(decoded.data(), decoded.size(), 0, &o, &added);
        sync.PushBack(o);
        last_mode = rc == webrtc::PreemptiveExpand::kSuccess
                        ? LM_PE_SUCCESS
                        : (rc == webrtc::PreemptiveExpand::kSuccessLowEnergy
                               ? LM_PE_LOW
                               : LM_PE_FAIL);
        expand.Reset();
        break;
      }
    }

    // Extract one 10 ms output block, mirroring get_audio's output stage.
    if (sync.FutureLength() < kOut) PushZeros(sync, kOut - sync.FutureLength());
    size_t ni = sync.next_index();
    for (size_t i = 0; i < kOut; ++i) out_stream.push_back(sync[0][ni + i]);
    sync.set_next_index(ni + kOut);
    if (sync.FutureLength() < kOverlap) {
      sync.set_next_index(sync.next_index() - (kOverlap - sync.FutureLength()));
    }
    if (last_mode == LM_NORMAL || last_mode == LM_ACC_FAIL ||
        last_mode == LM_PE_FAIL) {
      bg.Update(sync);
    }
  }

  Dump("pipeline_out", {"concatenated 10 ms blocks across the operation tape"},
       out_stream);
  std::vector<int16_t> after(sync.Size());
  sync.Channel(0).CopyTo(sync.Size(), 0, after.data());
  Dump("pipeline_sync_after", {"sync buffer after the full tape"}, after);
}

}  // namespace

int main(int argc, char** argv) {
  if (argc > 1) g_out_dir = argv[1];

  CaseCrossCorrelation();
  CaseCrossCorrelationAutoShift();
  CaseAutoCorrelation();
  CaseLevinsonDurbin();
  CaseFilterArFastQ12();
  CaseSqrtFloor();
  CaseDivW32W16();
  CaseRandomVector();
  CaseDownsampleTo4kHz();
  CasePeakDetection();
  CaseParabolicFit();
  CaseCrossFade();
  CaseBackgroundNoise();
  CaseAccelerate();
  CasePreemptiveExpand();
  CaseExpand();
  CaseNormal();
  CaseMerge();
  CasePipeline();
  return 0;
}
