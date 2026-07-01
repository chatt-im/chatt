#ifndef CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_MINMAX_H_
#define CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_MINMAX_H_

#include <algorithm>
#include <type_traits>

namespace webrtc {

template <typename R, typename T, typename L, typename H>
inline constexpr R SafeClamp(T value, L min, H max) {
  using C = std::common_type_t<T, L, H>;
  C clamped = std::clamp(static_cast<C>(value), static_cast<C>(min),
                         static_cast<C>(max));
  return static_cast<R>(clamped);
}

template <typename T, typename L, typename H>
inline constexpr std::common_type_t<T, L, H> SafeClamp(T value, L min, H max) {
  using R = std::common_type_t<T, L, H>;
  return std::clamp(static_cast<R>(value), static_cast<R>(min),
                    static_cast<R>(max));
}

}  // namespace webrtc

#endif  // CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_MINMAX_H_
