// Telemetry and comfort-noise stubs for the NetEQ DSP oracle.
//
// `statistics_calculator.cc` emits UMA histograms and `normal.cc` has an
// RFC3389 comfort-noise branch. Neither path affects DSP sample output for the
// deterministic inputs this oracle drives (no histogram readout, no CNG mode),
// so these symbols resolve to no-ops instead of pulling in the metrics,
// threading, and codec-database object graphs.

#include <cstdint>
#include <span>

#include "absl/strings/string_view.h"
#include "modules/audio_coding/codecs/cng/webrtc_cng.h"
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

bool ComfortNoiseDecoder::Generate(std::span<int16_t>, bool) { return false; }

}  // namespace webrtc
