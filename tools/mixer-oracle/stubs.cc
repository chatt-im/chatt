// Telemetry stubs for the mixer oracle.
//
// The AGC2 limiter records UMA-style histogram counters while processing. The
// counters do not affect sample output, so the standalone extractor resolves
// them to no-ops instead of pulling in WebRTC's metrics object graph.

#include "system_wrappers/include/metrics.h"

namespace webrtc {
namespace metrics {

Histogram* HistogramFactoryGetCounts(absl::string_view, int, int, int) {
  return nullptr;
}

Histogram* HistogramFactoryGetCountsLinear(absl::string_view, int, int, int) {
  return nullptr;
}

Histogram* HistogramFactoryGetEnumeration(absl::string_view, int) {
  return nullptr;
}

Histogram* SparseHistogramFactoryGetEnumeration(absl::string_view, int) {
  return nullptr;
}

void HistogramAdd(Histogram*, int) {}

}  // namespace metrics
}  // namespace webrtc
