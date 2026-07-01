#ifndef CHATT_MIXER_ORACLE_RTC_BASE_CHECKS_H_
#define CHATT_MIXER_ORACLE_RTC_BASE_CHECKS_H_

#include <cstdio>
#include <cstdlib>

namespace webrtc {

inline void OracleCheckFailed(const char* expr, const char* file, int line) {
  std::fprintf(stderr, "CHECK failed at %s:%d: %s\n", file, line, expr);
  std::abort();
}

template <typename T>
inline T CheckedDivExact(T a, T b) {
  if (a % b != 0) {
    OracleCheckFailed("CheckedDivExact", __FILE__, __LINE__);
  }
  return a / b;
}

}  // namespace webrtc

#define RTC_CHECK(condition)                                             \
  do {                                                                   \
    if (!(condition)) {                                                  \
      ::webrtc::OracleCheckFailed(#condition, __FILE__, __LINE__);       \
    }                                                                    \
  } while (0)

#define RTC_CHECK_EQ(a, b) RTC_CHECK((a) == (b))
#define RTC_CHECK_NE(a, b) RTC_CHECK((a) != (b))
#define RTC_CHECK_LE(a, b) RTC_CHECK((a) <= (b))
#define RTC_CHECK_LT(a, b) RTC_CHECK((a) < (b))
#define RTC_CHECK_GE(a, b) RTC_CHECK((a) >= (b))
#define RTC_CHECK_GT(a, b) RTC_CHECK((a) > (b))

#define RTC_DCHECK(condition) ((void)0)
#define RTC_DCHECK_EQ(a, b) ((void)0)
#define RTC_DCHECK_NE(a, b) ((void)0)
#define RTC_DCHECK_LE(a, b) ((void)0)
#define RTC_DCHECK_LT(a, b) ((void)0)
#define RTC_DCHECK_GE(a, b) ((void)0)
#define RTC_DCHECK_GT(a, b) ((void)0)
#define RTC_DCHECK_NOTREACHED() ((void)0)

#define RTC_CHECK_NOTREACHED()                                           \
  do {                                                                   \
    ::webrtc::OracleCheckFailed("RTC_CHECK_NOTREACHED", __FILE__,        \
                                __LINE__);                               \
  } while (0)

#endif  // CHATT_MIXER_ORACLE_RTC_BASE_CHECKS_H_
