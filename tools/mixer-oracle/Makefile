# Standalone WebRTC mixer/limiter oracle used to regenerate committed
# reference vectors for chatt's mono 48 kHz playback FrameCombiner.
#
# This intentionally follows tools/neteq-oracle's extraction model: compile the
# specific WebRTC translation units needed for the math under test, with tiny
# local shims for checks/metrics/build support. No gn/gclient/direct WebRTC
# build is required.
#
# Prereqs:
#   - WebRTC source at $(WEBRTC) (default /tmp/webrtc)
#
# Usage:
#   make            # build ./oracle
#   make vectors    # regenerate ../../tests/mixer_vectors/*.txt

WEBRTC ?= /tmp/webrtc
CXX    := clang++

CXXFLAGS := -std=c++20 -O2 -DWEBRTC_POSIX -DWEBRTC_LINUX \
            -DWEBRTC_APM_DEBUG_DUMP=0 -DRTC_DISABLE_METRICS \
            -Icompat -I$(WEBRTC) \
            -Wno-nullability-completeness -Wno-deprecated

AGC2 := $(WEBRTC)/modules/audio_processing/agc2

WEBRTC_SRCS := \
  $(AGC2)/limiter.cc \
  $(AGC2)/fixed_digital_level_estimator.cc \
  $(AGC2)/interpolated_gain_curve.cc \
  $(WEBRTC)/modules/audio_processing/logging/apm_data_dumper.cc

LOCAL_SRCS := stubs.cc main.cc

oracle: $(WEBRTC_SRCS) $(LOCAL_SRCS)
	$(CXX) $(CXXFLAGS) $(WEBRTC_SRCS) $(LOCAL_SRCS) -o oracle

vectors: oracle
	mkdir -p ../../tests/mixer_vectors
	./oracle ../../tests/mixer_vectors

clean:
	rm -f oracle

.PHONY: vectors clean
