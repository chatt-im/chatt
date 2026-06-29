use jsony::Jsony;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct UserId(pub u32);

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
