use jiff::Timestamp;

/// The only source of "now" for the domain and the engine (AD-8).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}
