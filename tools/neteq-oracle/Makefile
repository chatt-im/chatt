# Standalone build of the WebRTC NetEQ DSP, used to dump byte-exact reference
# vectors for the Rust port. Compiles the specific signal_processing and neteq
# translation units against Abseil headers. No gn/gclient.
#
# Prereqs:
#   - WebRTC source at $(WEBRTC) (default /tmp/webrtc)
#   - Abseil development headers/libraries (system installation or checkout)
#   - See README.md for network-oracle and sanitizer prerequisites/commands
#
# Usage:
#   make            # build ./oracle
#   make vectors    # build then regenerate ../../tests/neteq_vectors/*.txt
#
# CasePipeline reads a real-audio fixture, tests/neteq_vectors/
# pipeline_audio_in.txt (one decimal int16 per line). It is large and NOT
# committed; generate it on demand from the repo's sample clip with:
#   ffmpeg -v error -i assets/sample-001.opus -f s16le -ar 48000 -ac 1 - | \
#     python3 -c 'import sys,array; a=array.array("h"); \
#       a.frombytes(sys.stdin.buffer.read()); \
#       print(chr(10).join(str(x) for x in a))' \
#     > ../../tests/neteq_vectors/pipeline_audio_in.txt
# Then `make vectors` writes pipeline_out.txt / pipeline_sync_after.txt and the
# Rust test `pipeline_matches_oracle` asserts them byte-for-byte. The tape
# repeats over as many whole cycles as the clip allows, so the entire audio is
# exercised. The Rust test is ignored by default and requires the fixture when
# explicitly invoked with --ignored. README.md documents /tmp vector output.
#
# Everything compiles in one clang++ invocation. The source set mixes .c and
# .cc translation units that share basenames (e.g. signal_processing's
# cross_correlation.c vs neteq's cross_correlation.cc), so intermediate object
# files would collide; a single command sidesteps that.

WEBRTC ?= /tmp/webrtc
ABSEIL := $(WEBRTC)/third_party/abseil-cpp
CXX    := clang++

CXXFLAGS := -std=c++20 -O2 -DWEBRTC_POSIX -DWEBRTC_LINUX \
            -I$(WEBRTC) -I$(ABSEIL) \
            -Wno-nullability-completeness -Wno-deprecated

SPL := $(WEBRTC)/common_audio/signal_processing
SPL_SRCS := \
  $(SPL)/cross_correlation.c \
  $(SPL)/auto_correlation.c \
  $(SPL)/levinson_durbin.c \
  $(SPL)/filter_ar_fast_q12.c \
  $(SPL)/filter_ma_fast_q12.c \
  $(SPL)/randomization_functions.c \
  $(SPL)/division_operations.c \
  $(SPL)/energy.c \
  $(SPL)/vector_scaling_operations.c \
  $(SPL)/min_max_operations.c \
  $(SPL)/downsample_fast.c \
  $(SPL)/get_scaling_square.c \
  $(SPL)/spl_sqrt.c \
  $(SPL)/spl_init.c \
  $(SPL)/copy_set_operations.c \
  $(SPL)/vector_operations.c \
  $(SPL)/dot_product_with_scale.cc

NETEQ := $(WEBRTC)/modules/audio_coding/neteq
NETEQ_SRCS := \
  $(NETEQ)/expand.cc \
  $(NETEQ)/merge.cc \
  $(NETEQ)/normal.cc \
  $(NETEQ)/accelerate.cc \
  $(NETEQ)/preemptive_expand.cc \
  $(NETEQ)/time_stretch.cc \
  $(NETEQ)/background_noise.cc \
  $(NETEQ)/random_vector.cc \
  $(NETEQ)/cross_correlation.cc \
  $(NETEQ)/dsp_helper.cc \
  $(NETEQ)/audio_vector.cc \
  $(NETEQ)/audio_multi_vector.cc \
  $(NETEQ)/sync_buffer.cc \
  $(NETEQ)/statistics_calculator.cc \
  $(NETEQ)/expand_uma_logger.cc

RTC_SRCS := \
  $(WEBRTC)/rtc_base/checks.cc \
  $(WEBRTC)/api/neteq/tick_timer.cc

LOCAL_SRCS := stubs.cc main.cc

ALL_SRCS := $(SPL_SRCS) $(NETEQ_SRCS) $(RTC_SRCS) $(LOCAL_SRCS)

# spl_sqrt_floor.h has no extern "C" guard, so compiling its .c as C++ (the
# clang++ default for the driver) would mangle WebRtcSpl_SqrtFloor and break the
# link against the C-linkage callers. Build it as C and link the object.
SQRT_SRC := $(WEBRTC)/common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c
SQRT_OBJ := build/spl_sqrt_floor.o

$(SQRT_OBJ): $(SQRT_SRC)
	@mkdir -p build
	clang -std=c11 -O2 -I$(WEBRTC) -c $< -o $@

oracle: $(ALL_SRCS) $(SQRT_OBJ)
	$(CXX) $(CXXFLAGS) $(ALL_SRCS) $(SQRT_OBJ) -o oracle

vectors: oracle
	./oracle ../../tests/neteq_vectors

clean:
	rm -rf oracle network-oracle network-oracle-sanitized build

.PHONY: vectors clean

# Link only the reference control-plane translation units. No algorithm stubs.
NETWORK_SRCS := network.cc \
  $(NETEQ)/packet.cc $(NETEQ)/packet_buffer.cc $(NETEQ)/statistics_calculator.cc \
  $(NETEQ)/decoder_database.cc $(WEBRTC)/system_wrappers/source/metrics.cc \
  $(NETEQ)/expand_uma_logger.cc $(WEBRTC)/api/audio_codecs/audio_format.cc \
  $(WEBRTC)/rtc_base/ref_counter.cc \
  $(WEBRTC)/modules/audio_coding/codecs/cng/webrtc_cng.cc \
  $(NETEQ)/decision_logic.cc $(NETEQ)/delay_manager.cc \
  $(NETEQ)/delay_constraints.cc $(NETEQ)/buffer_level_filter.cc \
  $(NETEQ)/packet_arrival_history.cc $(NETEQ)/histogram.cc \
  $(NETEQ)/underrun_optimizer.cc $(NETEQ)/reorder_optimizer.cc \
  $(WEBRTC)/api/neteq/tick_timer.cc $(WEBRTC)/rtc_base/checks.cc \
  $(WEBRTC)/rtc_base/experiments/struct_parameters_parser.cc \
  $(WEBRTC)/rtc_base/experiments/field_trial_parser.cc \
  $(WEBRTC)/rtc_base/experiments/field_trial_units.cc \
  $(WEBRTC)/rtc_base/logging.cc $(WEBRTC)/rtc_base/string_encode.cc \
  $(WEBRTC)/rtc_base/string_utils.cc $(WEBRTC)/rtc_base/time_utils.cc \
  $(WEBRTC)/rtc_base/platform_thread_types.cc \
  $(WEBRTC)/rtc_base/strings/string_builder.cc \
  $(WEBRTC)/rtc_base/system_time.cc $(WEBRTC)/api/task_queue/task_queue_base.cc

network-oracle: $(NETWORK_SRCS)
	$(CXX) $(CXXFLAGS) -ffunction-sections -fdata-sections $(NETWORK_SRCS) \
	  -Wl,--gc-sections $$(pkg-config --libs absl_strings absl_str_format) -pthread -o $@

network-oracle-sanitized: $(NETWORK_SRCS)
	$(CXX) $(CXXFLAGS) -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all \
	  -ffunction-sections -fdata-sections $(NETWORK_SRCS) \
	  -Wl,--gc-sections $$(pkg-config --libs absl_strings absl_str_format) -pthread -o $@
