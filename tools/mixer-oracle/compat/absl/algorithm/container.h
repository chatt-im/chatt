#ifndef CHATT_MIXER_ORACLE_ABSL_ALGORITHM_CONTAINER_H_
#define CHATT_MIXER_ORACLE_ABSL_ALGORITHM_CONTAINER_H_

#include <algorithm>
#include <iterator>

namespace absl {

template <typename Range, typename T>
void c_fill(Range&& range, const T& value) {
  std::fill(std::begin(range), std::end(range), value);
}

}  // namespace absl

#endif  // CHATT_MIXER_ORACLE_ABSL_ALGORITHM_CONTAINER_H_
