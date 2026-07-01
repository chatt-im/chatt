#ifndef CHATT_MIXER_ORACLE_SYSTEM_WRAPPERS_INCLUDE_METRICS_H_
#define CHATT_MIXER_ORACLE_SYSTEM_WRAPPERS_INCLUDE_METRICS_H_

#include "absl/strings/string_view.h"

namespace webrtc {
namespace metrics {

class Histogram;

Histogram* HistogramFactoryGetCounts(absl::string_view name,
                                     int min,
                                     int max,
                                     int bucket_count);
Histogram* HistogramFactoryGetCountsLinear(absl::string_view name,
                                           int min,
                                           int max,
                                           int bucket_count);
Histogram* HistogramFactoryGetEnumeration(absl::string_view name,
                                          int boundary);
Histogram* SparseHistogramFactoryGetEnumeration(absl::string_view name,
                                                int boundary);
void HistogramAdd(Histogram* histogram_pointer, int sample);

}  // namespace metrics
}  // namespace webrtc

#endif  // CHATT_MIXER_ORACLE_SYSTEM_WRAPPERS_INCLUDE_METRICS_H_
