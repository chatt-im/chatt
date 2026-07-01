#ifndef CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_CONVERSIONS_H_
#define CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_CONVERSIONS_H_

namespace webrtc {

template <typename Dst, typename Src>
inline constexpr Dst checked_cast(Src value) {
  return static_cast<Dst>(value);
}

template <typename Dst, typename Src>
inline constexpr Dst dchecked_cast(Src value) {
  return static_cast<Dst>(value);
}

template <typename Dst, typename Src>
inline constexpr Dst saturated_cast(Src value) {
  return static_cast<Dst>(value);
}

}  // namespace webrtc

#endif  // CHATT_MIXER_ORACLE_RTC_BASE_NUMERICS_SAFE_CONVERSIONS_H_
