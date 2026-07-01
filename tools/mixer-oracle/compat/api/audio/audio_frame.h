#ifndef CHATT_MIXER_ORACLE_API_AUDIO_AUDIO_FRAME_H_
#define CHATT_MIXER_ORACLE_API_AUDIO_AUDIO_FRAME_H_

#include <cstddef>

namespace webrtc {

constexpr size_t kDefaultAudioBufferLengthMs = 10;
constexpr size_t kDefaultAudioBuffersPerSec =
    1000u / kDefaultAudioBufferLengthMs;

}  // namespace webrtc

#endif  // CHATT_MIXER_ORACLE_API_AUDIO_AUDIO_FRAME_H_
