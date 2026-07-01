#ifndef CHATT_MIXER_ORACLE_RTC_BASE_STRINGS_STRING_BUILDER_H_
#define CHATT_MIXER_ORACLE_RTC_BASE_STRINGS_STRING_BUILDER_H_

#include <string>

#include "absl/strings/string_view.h"

namespace webrtc {

class StringBuilder {
 public:
  StringBuilder() = default;
  explicit StringBuilder(absl::string_view s) : str_(s) {}

  StringBuilder& operator<<(absl::string_view s) {
    str_.append(s);
    return *this;
  }

  StringBuilder& operator<<(const char* s) {
    str_.append(s);
    return *this;
  }

  const std::string& str() const { return str_; }

 private:
  std::string str_;
};

}  // namespace webrtc

#endif  // CHATT_MIXER_ORACLE_RTC_BASE_STRINGS_STRING_BUILDER_H_
