//! Tune (#5643): ASIO DSD I/O format switching (`ASIOFuture` selectors).
//!
//! Self-contained on purpose: no `crate::` path, no generated binding. Tune
//! compiles this very file on Linux (`tune-core/tests/asio_dsd_5643.rs`, via
//! `#[path]`) to test it without the ASIO SDK.
//!
//! Values copied from the Steinberg ASIO SDK `asio.h`:
//!
//! ```c
//! kAsioSetIoFormat   = 0x23111961, /* ASIOIoFormat * in params. */
//! kAsioGetIoFormat   = 0x23111983, /* ASIOIoFormat * in params. */
//! kAsioCanDoIoFormat = 0x23112004, /* ASIOIoFormat * in params. */
//!
//! typedef enum ASIOIoFormatType_e {
//!     kASIOFormatInvalid = -1,
//!     kASIOPCMFormat = 0,
//!     kASIODSDFormat = 1,
//! } ASIOIoFormatType;
//!
//! typedef struct ASIOIoFormat_s {
//!     ASIOIoFormatType FormatType;
//!     char future[512 - sizeof(ASIOIoFormatType)];
//! } ASIOIoFormat;
//! ```

use std::os::raw::{c_char, c_int};

/// `ASIOFuture` selector: switch the driver between PCM and DSD.
pub const K_ASIO_SET_IO_FORMAT: i32 = 0x2311_1961;
/// `ASIOFuture` selector: read the driver's current I/O format.
pub const K_ASIO_GET_IO_FORMAT: i32 = 0x2311_1983;
/// `ASIOFuture` selector: ask whether the driver accepts an I/O format.
pub const K_ASIO_CAN_DO_IO_FORMAT: i32 = 0x2311_2004;

/// `ASE_OK`: generic success.
pub const ASE_OK: i32 = 0;
/// `ASE_SUCCESS`: the success value specific to `ASIOFuture`.
pub const ASE_SUCCESS: i32 = 0x3f48_47a0;
/// `ASE_NotPresent`.
pub const ASE_NOT_PRESENT: i32 = -1000;
/// `ASE_InvalidParameter`: what a driver unaware of a selector returns.
pub const ASE_INVALID_PARAMETER: i32 = -998;

/// `ASIOIoFormatType`, without its `kASIOFormatInvalid` sentinel.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AsioIoFormatType {
    /// `kASIOPCMFormat`.
    Pcm,
    /// `kASIODSDFormat`.
    Dsd,
}

/// `kASIOFormatInvalid`.
pub const K_ASIO_FORMAT_INVALID: c_int = -1;

impl AsioIoFormatType {
    /// The SDK value of this format.
    pub const fn raw(self) -> c_int {
        match self {
            AsioIoFormatType::Pcm => 0,
            AsioIoFormatType::Dsd => 1,
        }
    }

    /// Reads an SDK value; `None` for `kASIOFormatInvalid` or anything unknown.
    pub const fn from_raw(raw: c_int) -> Option<Self> {
        match raw {
            0 => Some(AsioIoFormatType::Pcm),
            1 => Some(AsioIoFormatType::Dsd),
            _ => None,
        }
    }
}

/// Mirror of the SDK's `ASIOIoFormat` (512 bytes, passed by pointer to
/// `ASIOFuture`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioIoFormat {
    pub format_type: c_int,
    pub future: [c_char; 512 - std::mem::size_of::<c_int>()],
}

impl AsioIoFormat {
    /// A request for the given format, reserved bytes zeroed.
    pub const fn new(format_type: AsioIoFormatType) -> Self {
        AsioIoFormat {
            format_type: format_type.raw(),
            future: [0; 512 - std::mem::size_of::<c_int>()],
        }
    }

    /// An output slot for `kAsioGetIoFormat`, preset to `kASIOFormatInvalid`
    /// so that a driver which writes nothing is not read as PCM.
    pub const fn invalid() -> Self {
        AsioIoFormat {
            format_type: K_ASIO_FORMAT_INVALID,
            future: [0; 512 - std::mem::size_of::<c_int>()],
        }
    }
}

impl std::fmt::Debug for AsioIoFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsioIoFormat")
            .field("format_type", &self.format_type)
            .finish_non_exhaustive()
    }
}

/// Reads the result of `ASIOFuture(kAsioCanDoIoFormat, ..)`.
///
/// `Some(true)`: the driver accepts the format (`ASE_SUCCESS`, or `ASE_OK`
/// as `asio_result!` already accepts it). `Some(false)`: the driver refuses
/// it or does not know the selector (`ASE_NotPresent`,
/// `ASE_InvalidParameter`, the two answers the SDK documents for an
/// unsupported `ASIOFuture`). `None`: any other code, to be reported as an
/// error by the caller.
pub const fn can_do_from_code(code: i32) -> Option<bool> {
    match code {
        ASE_SUCCESS | ASE_OK => Some(true),
        ASE_NOT_PRESENT | ASE_INVALID_PARAMETER => Some(false),
        _ => None,
    }
}
