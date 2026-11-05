//! Wall time for provenance; sleep-aware monotonic time for elapsed durations.
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use uuid::Uuid;
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}
fn continuous_ms() -> i64 {
    static RATIO: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *RATIO.get_or_init(|| {
        let mut info = Timebase { numer: 0, denom: 0 };
        // SAFETY: macOS fills the live, correctly laid out timebase structure.
        assert_eq!(unsafe { mach_timebase_info(&mut info) }, 0);
        assert_ne!(info.denom, 0);
        (info.numer, info.denom)
    });
    // SAFETY: the documented clock has no arguments and advances through sleep.
    let ticks = unsafe { mach_continuous_time() };
    ((u128::from(ticks) * u128::from(numer)) / u128::from(denom) / 1_000_000) as i64
}
fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
struct Anchor {
    epoch: i64,
    mono: i64,
    boot: String,
}
fn anchor() -> &'static Anchor {
    ANCHOR.get_or_init(|| Anchor {
        epoch: wall_ms(),
        mono: continuous_ms(),
        boot: Uuid::now_v7().to_string(),
    })
}
static ANCHOR: OnceLock<Anchor> = OnceLock::new();
pub fn initialize(minimum_position: i64) {
    ANCHOR.get_or_init(|| Anchor {
        epoch: wall_ms().max(minimum_position),
        mono: continuous_ms(),
        boot: Uuid::now_v7().to_string(),
    });
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stamp {
    pub epoch_ms: i64,
    pub mono_ms: i64,
    pub boot_id: String,
    pub timeline_ms: i64,
}
impl Stamp {
    pub fn now() -> Self {
        let a = anchor();
        let mono = continuous_ms();
        Self {
            epoch_ms: wall_ms(),
            mono_ms: mono,
            boot_id: a.boot.clone(),
            timeline_ms: a.epoch + (mono - a.mono).max(0),
        }
    }
    pub fn elapsed(&self, start: &Self) -> (i64, bool) {
        if self.boot_id == start.boot_id {
            ((self.mono_ms - start.mono_ms).max(0), false)
        } else {
            ((self.epoch_ms - start.epoch_ms).max(0), true)
        }
    }
}
pub fn register(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS;
    connection.create_scalar_function("intercat_mono", 0, flags, |_| Ok(Stamp::now().mono_ms))?;
    connection.create_scalar_function("intercat_boot", 0, flags, |_| Ok(Stamp::now().boot_id))?;
    connection.create_scalar_function("intercat_position", 0, flags, |_| {
        Ok(Stamp::now().timeline_ms)
    })?;
    Ok(())
}
