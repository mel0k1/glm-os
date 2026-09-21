//! Limine boot protocol requests for GLM OS.
//! All statics live in the `.limine_requests` section, bracketed by markers.

use limine::request::{
    BootloaderInfoRequest, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    RequestsEndMarker, RequestsStartMarker,
};
use limine::BaseRevision;

#[used]
#[link_section = ".limine_requests_start"]
pub static MARKER_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".limine_requests"]
pub static BASE_REVISION: BaseRevision = BaseRevision::new();

#[used]
#[link_section = ".limine_requests"]
pub static FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

#[used]
#[link_section = ".limine_requests"]
pub static HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

#[used]
#[link_section = ".limine_requests"]
pub static MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[link_section = ".limine_requests"]
pub static MODULE_REQUEST: ModuleRequest = ModuleRequest::new();

#[used]
#[link_section = ".limine_requests"]
pub static BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[link_section = ".limine_requests_end"]
pub static MARKER_END: RequestsEndMarker = RequestsEndMarker::new();

/// Higher-half direct map offset (physical + offset = virtual).
pub fn hhdm_offset() -> u64 {
    HHDM_REQUEST
        .get_response()
        .map(|r| r.offset())
        .unwrap_or(0)
}

/// Bootloader version string (e.g. "12.9.0").
pub fn bootloader_version() -> Option<&'static str> {
    BOOTLOADER_INFO_REQUEST
        .get_response()
        .map(|r| r.version())
}
