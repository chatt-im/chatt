//! Port of WebRTC's `RandomVector` (neteq), the deterministic excitation source
//! for Expand and BackgroundNoise. A 256-entry table is indexed by a running
//! seed so concealment output is reproducible.

/// The 256-entry pseudo-random table (`RandomVector::kRandomTable`).
pub(crate) const RANDOM_TABLE: [i16; 256] = [
    2680, 5532, 441, 5520, 16170, -5146, -1024, -8733, 3115, 9598, -10380, -4959, -1280, -21716,
    7133, -1522, 13458, -3902, 2789, -675, 3441, 5016, -13599, -4003, -2739, 3922, -7209, 13352,
    -11617, -7241, 12905, -2314, 5426, 10121, -9702, 11207, -13542, 1373, 816, -5934, -12504, 4798,
    1811, 4112, -613, 201, -10367, -2960, -2419, 3442, 4299, -6116, -6092, 1552, -1650, -480,
    -1237, 18720, -11858, -8303, -8212, 865, -2890, -16968, 12052, -5845, -5912, 9777, -5665,
    -6294, 5426, -4737, -6335, 1652, 761, 3832, 641, -8552, -9084, -5753, 8146, 12156, -4915,
    15086, -1231, -1869, 11749, -9319, -6403, 11407, 6232, -1683, 24340, -11166, 4017, -10448,
    3153, -2936, 6212, 2891, -866, -404, -4807, -2324, -1917, -2388, -6470, -3895, -10300, 5323,
    -5403, 2205, 4640, 7022, -21186, -6244, -882, -10031, -3395, -12885, 7155, -5339, 5079, -2645,
    -9515, 6622, 14651, 15852, 359, 122, 8246, -3502, -6696, -3679, -13535, -1409, -704, -7403,
    -4007, 1798, 279, -420, -12796, -14219, 1141, 3359, 11434, 7049, -6684, -7473, 14283, -4115,
    -9123, -8969, 4152, 4117, 13792, 5742, 16168, 8661, -1609, -6095, 1881, 14380, -5588, 6758,
    -6425, -22969, -7269, 7031, 1119, -1611, -5850, -11281, 3559, -8952, -10146, -4667, -16251,
    -1538, 2062, -1012, -13073, 227, -3142, -5265, 20, 5770, -7559, 4740, -4819, 992, -8208, -7130,
    -4652, 6725, 7369, -1036, 13144, -1588, -5304, -2344, -449, -5705, -8894, 5205, -17904, -11188,
    -1022, 4852, 10101, -5255, -4200, -752, 7941, -1543, 5959, 14719, 13346, 17045, -15605, -1678,
    -1600, -9230, 68, 23348, 1172, 7750, 11212, -18227, 9956, 4161, 883, 3947, 4341, 1014, -4889,
    -2603, 1246, -5630, -3596, -870, -1298, 2784, -3317, -6612, -20541, 4166, 4181, -8625, 3562,
    12890, 4761, 3205, -12259, -8579,
];

const TABLE_SIZE: usize = RANDOM_TABLE.len();

/// `RandomVector`. Seed and increment reproduce the reference exactly.
#[derive(Debug)]
pub(crate) struct RandomVector {
    seed: u32,
    seed_increment: i16,
}

impl RandomVector {
    pub(crate) fn new() -> Self {
        Self {
            seed: 777,
            seed_increment: 1,
        }
    }

    #[cfg(test)]
    pub(crate) fn reset(&mut self) {
        self.seed = 777;
        self.seed_increment = 1;
    }

    /// `Generate`: fill `output` from the table, advancing the seed per sample.
    pub(crate) fn generate(&mut self, output: &mut [i16]) {
        for sample in output.iter_mut() {
            self.seed = self.seed.wrapping_add(self.seed_increment as u32);
            let position = (self.seed & (TABLE_SIZE as u32 - 1)) as usize;
            *sample = RANDOM_TABLE[position];
        }
    }

    /// `IncreaseSeedIncrement`: bump the per-sample seed step, wrapped to 8 bits.
    pub(crate) fn increase_seed_increment(&mut self, increase_by: i16) {
        self.seed_increment = (self.seed_increment + increase_by) & (TABLE_SIZE as i16 - 1);
    }

    /// `RandomVector::set_seed_increment`. Used by `Expand::AnalyzeSignal`.
    pub(crate) fn set_seed_increment(&mut self, value: i16) {
        self.seed_increment = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    #[test]
    fn generate_matches_oracle() {
        let mut rv = RandomVector::new();
        rv.reset();
        let mut a = [0i16; 120];
        rv.generate(&mut a);
        rv.increase_seed_increment(2);
        let mut b = [0i16; 120];
        rv.generate(&mut b);

        let mut got: Vec<i64> = a.iter().map(|&x| x as i64).collect();
        got.extend(b.iter().map(|&x| x as i64));
        assert_eq!(got, load("random_vector_generate"));
    }
}
