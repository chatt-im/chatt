//! Executable differential tests against actual C++ WebRTC sources.
//! See tools/neteq-oracle/README.md for prerequisites and reproducible runs.
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::decision_logic::DecisionLogic;
use super::neteq_status::{
    ControllerConfig, NetEqStatus, PacketArrivedInfo, PacketBufferInfo, PacketInfo,
};
use super::operation::{Mode, Operation};
use super::packet::{Packet, PacketPayload, Priority};
use super::packet_arrival_history::PacketArrivalHistory;
use super::packet_buffer::{InsertOutcome, PacketBuffer};
use super::tick_timer::TickTimer;

struct ReferenceState {
    history: PacketArrivalHistory,
    controller: DecisionLogic,
    timer: TickTimer,
    packets: PacketBuffer,
}

impl ReferenceState {
    fn new() -> Self {
        let timer = TickTimer::new();
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        // Match the reference's default startup target (80), not Chatt's live setting.
        let mut controller = DecisionLogic::new(
            ControllerConfig {
                allow_time_stretching: true,
                max_packets_in_buffer: 200,
                start_delay_ms: 80,
                min_delay_ms: 20,
                base_min_delay_ms: 0,
                max_delay_ms: 1000,
                resume_hold_max_ms: 0,
            },
            &timer,
        );
        controller.set_sample_rate(48000, 480);
        Self {
            history,
            controller,
            timer,
            packets: PacketBuffer::new(200),
        }
    }

    fn execute(&mut self, command: &str) -> Vec<i64> {
        let mut words = command.split_whitespace();
        let op = words.next().unwrap();
        let args: Vec<i64> = words.map(|s| s.parse().unwrap()).collect();
        let now = self.timer.ticks() as i64 * 480;
        match op {
            "T" => {
                self.timer.increment_by(args[0] as u64);
                vec![]
            }
            "F" => {
                self.history.reset();
                self.controller.soft_reset(&self.timer);
                vec![]
            }
            "I" => {
                let result = self.packets.insert_packet(
                    Packet::new(
                        args[0] as u32,
                        args[1] as u32,
                        Priority::new(args[2] as i32, args[3] as i32),
                        960,
                        PacketPayload::Opus(std::sync::Arc::new(vec![1])),
                    ),
                    &self.timer,
                );
                vec![
                    if result == InsertOutcome::Ok { 0 } else { 1 },
                    self.packets.num_packets() as i64,
                ]
            }
            "G" => {
                let packet = self.packets.get_next_packet();
                vec![
                    packet.as_ref().map_or(-1, |p| p.timestamp as i64),
                    packet.as_ref().map_or(-1, |p| p.sequence_number as i64),
                    packet
                        .as_ref()
                        .map_or(-1, |p| p.priority.codec_level as i64),
                    packet.as_ref().map_or(-1, |p| p.priority.red_level as i64),
                    self.packets.num_packets() as i64,
                ]
            }
            "H" => {
                let ts = args[0] as u32;
                vec![
                    self.history.insert(ts, args[1] as i32, now) as i64,
                    self.history.size() as i64,
                    self.history.delay_ms(ts, now) as i64,
                    self.history.max_delay_ms() as i64,
                    self.history.is_newest_rtp_timestamp(ts) as i64,
                ]
            }
            "A" => {
                let delay = self.controller.packet_arrived(
                    48000,
                    args[2] != 0,
                    &PacketArrivedInfo {
                        main_timestamp: args[0] as u32,
                        packet_length_samples: args[1] as usize,
                        is_cng_or_dtmf: false,
                        buffer_flush: args[3] != 0,
                    },
                    &self.timer,
                    now,
                );
                vec![
                    delay.unwrap_or(-1) as i64,
                    self.controller.target_level_ms() as i64,
                ]
            }
            "D" => {
                let modes = [
                    Mode::Normal,
                    Mode::Expand,
                    Mode::Merge,
                    Mode::AccelerateSuccess,
                    Mode::AccelerateFail,
                    Mode::PreemptiveExpandSuccess,
                    Mode::PreemptiveExpandFail,
                    Mode::Error,
                ];
                self.controller.set_prev_time_scale(args[10] != 0);
                self.controller.set_sample_memory(args[11] as i32);
                let status = NetEqStatus {
                    target_timestamp: args[0] as u32,
                    next_packet: (args[1] != 0).then_some(PacketInfo {
                        timestamp: args[2] as u32,
                        is_cng: false,
                    }),
                    last_mode: modes[args[3] as usize],
                    packet_buffer_info: PacketBufferInfo {
                        dtx_or_cng: false,
                        span_samples: args[4] as usize,
                        span_samples_wait_time: args[5] as usize,
                        num_packets: args[6] as usize,
                    },
                    sync_buffer_samples: args[7] as usize,
                    generated_noise_samples: args[8] as usize,
                    expand_mutefactor: args[9] as i16,
                    play_dtmf: false,
                };
                let op = match self.controller.get_decision(&status, &self.timer, now) {
                    Operation::Normal => 0,
                    Operation::Merge => 1,
                    Operation::Expand => 2,
                    Operation::Accelerate => 3,
                    Operation::FastAccelerate => 4,
                    Operation::PreemptiveExpand => 5,
                    Operation::Undefined => 6,
                    other => panic!("unsupported operation {other:?}"),
                };
                vec![
                    op,
                    self.controller.target_level_ms() as i64,
                    self.controller.noise_fast_forward() as i64,
                ]
            }
            _ => panic!("invalid command {command}"),
        }
    }
}

struct Corpus {
    dir: PathBuf,
    input: BufWriter<File>,
    expected: Vec<(u64, u64, u64)>, // hash, byte offset, byte length
    bytes: u64,
    events: usize,
}

impl Corpus {
    fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("chatt-neteq-{name}-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = BufWriter::new(File::create(dir.join("input.txt")).unwrap());
        Self {
            dir,
            input,
            expected: vec![],
            bytes: 0,
            events: 0,
        }
    }

    fn case(&mut self, commands: &[String]) {
        let mut state = ReferenceState::new();
        let mut hash = 14695981039346656037_u64;
        let script = format!("R {}\n{}\n", self.expected.len(), commands.join("\n"));
        for command in commands {
            for value in state.execute(command) {
                for byte in value.to_le_bytes() {
                    hash = (hash ^ byte as u64).wrapping_mul(1099511628211);
                }
            }
        }
        self.input.write_all(script.as_bytes()).unwrap();
        self.expected.push((hash, self.bytes, script.len() as u64));
        self.bytes += script.len() as u64;
        self.events += commands.len();
    }

    fn verify(mut self) {
        self.input.flush().unwrap();
        let oracle = std::env::var("CHATT_NETEQ_ORACLE")
            .expect("Build tools/neteq-oracle/network-oracle and set CHATT_NETEQ_ORACLE to its absolute path");
        let output = self.dir.join("reference.txt");
        let mut expected_file = BufWriter::new(File::create(self.dir.join("rust.txt")).unwrap());
        for (id, (hash, _, _)) in self.expected.iter().enumerate() {
            writeln!(expected_file, "{id} {hash}").unwrap();
        }
        expected_file.flush().unwrap();
        let status = Command::new(&oracle)
            .stdin(File::open(self.dir.join("input.txt")).unwrap())
            .stdout(File::create(&output).unwrap())
            .stderr(File::create(self.dir.join("reference.stderr")).unwrap())
            .status()
            .expect("start C++ oracle");
        assert!(
            status.success(),
            "C++ oracle failed; corpus: {}",
            self.dir.display()
        );
        let results = fs::read_to_string(output).unwrap();
        assert_eq!(
            results.lines().count(),
            self.expected.len(),
            "missing reference cases"
        );
        let mut mismatches = 0;
        for (id, line) in results.lines().enumerate() {
            let (reference_id, hash) = line.split_once(' ').unwrap();
            assert_eq!(reference_id.parse::<usize>().unwrap(), id);
            if hash.parse::<u64>().unwrap() == self.expected[id].0 {
                continue;
            }
            mismatches += 1;
            if mismatches > 3 {
                continue;
            }
            let (_, offset, length) = self.expected[id];
            let mut input = File::open(self.dir.join("input.txt")).unwrap();
            input.seek(SeekFrom::Start(offset)).unwrap();
            let mut script = String::new();
            input.take(length).read_to_string(&mut script).unwrap();
            let failure = self.dir.join(format!("case-{id}.txt"));
            fs::write(&failure, &script).unwrap();
            let verbose = Command::new(&oracle)
                .arg("--verbose")
                .stdin(File::open(&failure).unwrap())
                .stdout(Stdio::piped())
                .output()
                .unwrap();
            assert!(verbose.status.success());
            let actual = String::from_utf8(verbose.stdout).unwrap();
            let mut state = ReferenceState::new();
            for (index, (command, reference)) in
                script.lines().skip(1).zip(actual.lines()).enumerate()
            {
                let rust = state.execute(command);
                let cpp: Vec<i64> = reference
                    .split_whitespace()
                    .map(|s| s.parse().unwrap())
                    .collect();
                if rust != cpp {
                    eprintln!(
                        "case {id}, event {index}: {command}; Rust {rust:?}, C++ {cpp:?}; replay {}",
                        failure.display()
                    );
                    break;
                }
            }
        }
        eprintln!(
            "{} cases, {} events, {mismatches} mismatching cases; corpus {}",
            self.expected.len(),
            self.events,
            self.dir.display()
        );
        assert_eq!(
            mismatches, 0,
            "C++ differential mismatch; keep corpus for replay"
        );
        // Keep the corpus and oracle outputs on success too for reproducibility.
    }
}

struct Rng(u64);
impl Rng {
    fn range(&mut self, limit: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % limit
    }
}

#[test]
#[ignore = "requires an independently compiled WebRTC network-oracle"]
fn upstream_history_all_eight_packet_permutations() {
    fn permute(corpus: &mut Corpus, values: &mut [u32], index: usize) {
        if index != values.len() {
            for swap in index..values.len() {
                values.swap(index, swap);
                permute(corpus, values, index + 1);
                values.swap(index, swap);
            }
            return;
        }
        // Both sides of RTP wrap; varying duration and a longer duplicate after each arrival.
        let base = u32::MAX - 3 * 960;
        let mut commands = vec![];
        for &value in values.iter() {
            let ts = base.wrapping_add(value * 960);
            commands.extend([
                "T 1".into(),
                format!("H {ts} 480"),
                "T 1".into(),
                format!("H {ts} 960"),
            ]);
        }
        // Move the window to force old min/max entries to be evicted.
        commands.extend([
            "T 250".into(),
            format!("H {} 960", base.wrapping_add(120_000)),
        ]);
        corpus.case(&commands);
        // Independently reorder all four short originals and four longer
        // same-timestamp duplicates: 8! orders, including recovery arriving first.
        let mut commands = vec![];
        for &value in values.iter() {
            let ts = base.wrapping_add((value % 4) * 960);
            let length = if value < 4 { 480 } else { 960 };
            commands.extend(["T 1".into(), format!("H {ts} {length}")]);
        }
        commands.extend([
            "T 250".into(),
            format!("H {} 960", base.wrapping_add(120_000)),
        ]);
        corpus.case(&commands);
    }
    let mut corpus = Corpus::new("permutations");
    permute(&mut corpus, &mut [0, 1, 2, 3, 4, 5, 6, 7], 0);
    corpus.verify();
}

#[test]
#[ignore = "requires an independently compiled WebRTC network-oracle"]
fn upstream_seeded_network_arrivals() {
    let seeds: u64 = std::env::var("CHATT_NETEQ_SEEDS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(1000);
    let ticks: u64 = std::env::var("CHATT_NETEQ_TICKS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(6000);
    assert!(
        seeds > 0 && ticks >= 100,
        "require at least one seed and one second per seed"
    );
    let mut corpus = Corpus::new("network");
    for seed in 1..=seeds {
        let mut rng = Rng(seed);
        let base = u32::MAX - rng.range(48_000) as u32;
        let mut deliveries = vec![];
        // 60 seconds per seed. 100–300 ms talkspurts each second, occasional
        // continuous speech seeds, advancing RTP through suppressed silence.
        for tick in (0..ticks).step_by(2) {
            if seed % 4 != 0 && tick % 100 >= 10 + (seed % 3) * 10 {
                continue;
            }
            if rng.range(100) < 5 {
                continue;
            }
            let delay = match rng.range(100) {
                0..=79 => rng.range(3),
                80..=94 => rng.range(30),
                _ => rng.range(150),
            };
            let ts = base.wrapping_add((tick * 480) as u32);
            deliveries.push((tick + delay, ts));
            if rng.range(100) < 10 {
                deliveries.push((tick + delay + rng.range(30), ts));
            }
        }
        deliveries.sort_by_key(|&(time, _)| time);
        let mut commands = vec![];
        let mut last = 0;
        for (time, ts) in deliveries {
            commands.push(format!("T {}", time - last));
            commands.push(format!("H {ts} 960"));
            commands.push(format!("A {ts} 960 1 0"));
            last = time;
        }
        corpus.case(&commands);
    }
    corpus.verify();
}

#[test]
#[ignore = "requires an independently compiled WebRTC network-oracle"]
fn upstream_seeded_controller_statuses() {
    let mut corpus = Corpus::new("decisions");
    for seed in 1..=1000 {
        let mut rng = Rng(seed);
        let base = u32::MAX - 48_000;
        let mut commands = vec![];
        for tick in 0..1000_u32 {
            let ts = base.wrapping_add(tick * 480);
            commands.push("T 1".into());
            if tick % 2 == 0 {
                commands.push(format!("A {ts} 960 1 0"));
            }
            let target = ts.wrapping_sub(rng.range(100) as u32 * 480);
            let next = target.wrapping_add(((rng.range(11) as i32 - 5) * 480) as u32);
            let mode = rng.range(8);
            // Concealment advances the noise clock while the decoded timestamp
            // is stationary. Successful decode modes have no accumulated noise.
            let noise = if mode == 1 { rng.range(1001) * 480 } else { 0 };
            let packets = rng.range(50);
            commands.push(format!(
                "D {target} {} {next} {mode} {} {} {packets} {} {noise} {} {} {}",
                rng.range(5).min(1),
                packets * 960,
                packets * 960 + rng.range(4800),
                rng.range(5) * 480,
                rng.range(16385),
                rng.range(2),
                (rng.range(1921) as i64 - 960)
            ));
        }
        corpus.case(&commands);
    }
    corpus.verify();
}

#[test]
#[ignore = "requires an independently compiled WebRTC network-oracle"]
fn upstream_seeded_packet_buffer_ordering() {
    let mut corpus = Corpus::new("packet-buffer");
    for seed in 1..=1000 {
        let mut rng = Rng(seed);
        let base = u32::MAX - 48_000;
        let mut commands = vec![];
        // Same-timestamp collisions between primary/FEC/DRED priorities, with
        // wide reordering, RTP wrap, interspersed reads, and 200-packet overflow.
        // Sequence numbers stay within u16: Chatt deliberately uses u32 instead.
        for index in 0..2000_u32 {
            let slot = index / 2;
            let ts = base.wrapping_add(((slot as i32 - rng.range(150) as i32) * 960) as u32);
            commands.push(format!(
                "I {ts} {} {} {}",
                index,
                rng.range(3),
                rng.range(3)
            ));
            if rng.range(4) == 0 {
                commands.push("G".into());
            }
        }
        commands.extend(std::iter::repeat_n("G".into(), 201));
        corpus.case(&commands);
    }
    corpus.verify();
}

#[test]
#[ignore = "requires an independently compiled WebRTC network-oracle"]
fn upstream_six_hour_clock_drift_after_longer_duplicate() {
    let mut corpus = Corpus::new("clock-drift");
    let base = u32::MAX - 9600;
    let mut commands = vec![
        format!("H {base} 480"),
        "T 1".into(),
        format!("H {base} 960"),
    ];
    for packet in 1..=1_080_000_u32 {
        let ts = base.wrapping_add(packet * 960 - packet / 10);
        commands.extend(["T 2".into(), format!("H {ts} 960")]);
    }
    corpus.case(&commands);
    corpus.verify();
}
