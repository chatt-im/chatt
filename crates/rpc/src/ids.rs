use jsony::Jsony;

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony, toml_spanner::Toml,
)]
#[jsony(Binary)]
#[toml(Toml)]
pub struct UserId(pub u64);

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct RoomId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct SessionId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct MessageId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct StreamId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct FileTransferId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct BugReportId(pub u64);
