// Differential oracle: compile the actual WebRTC controller and arrival history.
// This driver only maps inputs/outputs; it does not reimplement their algorithms.
#include <cstdint>
#include <iostream>
#include <memory>
#include <string>

#include "api/field_trials_view.h"
#include "api/neteq/neteq.h"
#include "api/neteq/neteq_controller.h"
#include "api/neteq/tick_timer.h"
#include "modules/audio_coding/neteq/buffer_level_filter.h"
#include "modules/audio_coding/neteq/decision_logic.h"
#include "modules/audio_coding/neteq/delay_manager.h"
#include "modules/audio_coding/neteq/packet_arrival_history.h"
#include "modules/audio_coding/neteq/packet_buffer.h"
#include "modules/audio_coding/neteq/statistics_calculator.h"

class EmptyTrials : public webrtc::FieldTrialsView {
 public:
  std::string Lookup(absl::string_view) const override { return {}; }
};

struct State {
  EmptyTrials trials;
  webrtc::TickTimer timer;
  webrtc::PacketArrivalHistory history{&timer, 2000};
  webrtc::StatisticsCalculator stats{&timer};
  webrtc::PacketBuffer packets{trials, 200, &timer, &stats};
  webrtc::DecisionLogic controller{
      {true, false, 200, 0, &timer},
      std::make_unique<webrtc::DelayManager>(webrtc::DelayManager::Config(trials), &timer),
      std::make_unique<webrtc::BufferLevelFilter>()};
  State() {
    history.set_sample_rate(48000);
    controller.SetSampleRate(48000, 480);
    controller.SetMinimumDelay(20);
    controller.SetMaximumDelay(1000);
  }
};

int Operation(webrtc::NetEq::Operation op) {
  using O = webrtc::NetEq::Operation;
  switch (op) {
    case O::kNormal: return 0;
    case O::kMerge: return 1;
    case O::kExpand: return 2;
    case O::kAccelerate: return 3;
    case O::kFastAccelerate: return 4;
    case O::kPreemptiveExpand: return 5;
    case O::kUndefined: return 6;
    default: std::abort();  // CNG/DTMF are outside Chatt's supported domain.
  }
}

int main(int argc, char** argv) {
  std::ios::sync_with_stdio(false);
  const bool verbose = argc > 1 && std::string(argv[1]) == "--verbose";
  using M = webrtc::NetEq::Mode;
  const M modes[] = {M::kNormal, M::kExpand, M::kMerge, M::kAccelerateSuccess,
                    M::kAccelerateFail, M::kPreemptiveExpandSuccess,
                    M::kPreemptiveExpandFail, M::kError};
  std::unique_ptr<State> state;
  uint64_t id = 0, hash = 14695981039346656037ULL;
  auto emit = [&](int64_t value) {
    if (verbose) std::cout << value << ' ';
    uint64_t bits = static_cast<uint64_t>(value);
    for (int byte = 0; byte < 8; ++byte) {
      hash = (hash ^ ((bits >> (8 * byte)) & 255)) * 1099511628211ULL;
    }
  };
  char command;
  while (std::cin >> command) {
    if (command == 'R') {
      if (state && !verbose) std::cout << id << ' ' << hash << '\n';
      std::cin >> id;
      state = std::make_unique<State>();
      hash = 14695981039346656037ULL;
      continue;
    }
    if (!state) return 2;
    if (command == 'T') {
      uint64_t ticks; std::cin >> ticks; state->timer.Increment(ticks);
    } else if (command == 'H') {
      uint32_t ts; int length; std::cin >> ts >> length;
      emit(state->history.Insert(ts, length));
      emit(state->history.size());
      emit(state->history.GetDelayMs(ts));
      emit(state->history.GetMaxDelayMs());
      emit(state->history.IsNewestRtpTimestamp(ts));
    } else if (command == 'A') {
      webrtc::NetEqController::PacketArrivedInfo info{};
      int update, flush;
      std::cin >> info.main_timestamp >> info.packet_length_samples >> update >> flush;
      info.buffer_flush = flush;
      auto delay = state->controller.PacketArrived(48000, update, info);
      emit(delay.value_or(-1)); emit(state->controller.TargetLevelMs());
    } else if (command == 'D') {
      webrtc::NetEqController::NetEqStatus status{};
      uint32_t next; int present, mode, previous, memory;
      std::cin >> status.target_timestamp >> present >> next >> mode
          >> status.packet_buffer_info.span_samples
          >> status.packet_buffer_info.span_samples_wait_time
          >> status.packet_buffer_info.num_packets >> status.sync_buffer_samples
          >> status.generated_noise_samples >> status.expand_mutefactor >> previous >> memory;
      if (present) status.next_packet = {next, false, false};
      if (mode < 0 || mode > 7) return 2;
      status.last_mode = modes[mode];
      state->controller.set_prev_time_scale(previous);
      state->controller.set_sample_memory(memory);
      bool reset = false;
      emit(Operation(state->controller.GetDecision(status, &reset)));
      emit(state->controller.TargetLevelMs());
      emit(state->controller.noise_fast_forward());
    } else if (command == 'F') {
      state->history.Reset(); state->controller.SoftReset();
    } else if (command == 'I') {
      webrtc::Packet packet;
      std::cin >> packet.timestamp >> packet.sequence_number
          >> packet.priority.codec_level >> packet.priority.red_level;
      packet.payload_type = 0;
      const uint8_t payload[] = {1};
      packet.payload.SetData(payload, 1);
      emit(state->packets.InsertPacket(std::move(packet), 960, 48000, 80));
      emit(state->packets.NumPacketsInBuffer());
    } else if (command == 'G') {
      auto packet = state->packets.GetNextPacket();
      emit(packet ? packet->timestamp : -1LL);
      emit(packet ? packet->sequence_number : -1LL);
      emit(packet ? packet->priority.codec_level : -1LL);
      emit(packet ? packet->priority.red_level : -1LL);
      emit(state->packets.NumPacketsInBuffer());
    } else { return 2; }
    if (!std::cin) return 2;
    if (verbose) std::cout << '\n';
  }
  if (state && !verbose) std::cout << id << ' ' << hash << '\n';
}
