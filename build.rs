// #58: delay-load `wpcap.dll` on Windows.
//
// GitHub-hosted `windows-latest` runners carry only the Npcap SDK (headers +
// link-time `.lib` import stubs, installed by our own CI "Install Npcap SDK"
// step) -- never the Npcap RUNTIME (`wpcap.dll` / `Packet.dll`). Installing
// the runtime needs a paid Npcap OEM subscription with silent-install
// support: the free Npcap installer has no silent-install switch at all
// (interactive licence click required), and even rust-pcap/pcap's own
// official CI (`01-build-and-test-windows.yml`) installs Npcap OEM via
// `NPCAP_OEM_USERNAME`/`NPCAP_OEM_PASSWORD` secrets tied to that paid
// licence -- so option 1 from #58 ("install the runtime") is genuinely
// impossible here without that purchase (evidence: gh issue comment on #58).
//
// Without this, `pcap`'s `#[link(name = "wpcap")]` (src/raw.rs) puts a hard
// PE import for `wpcap.dll` in every binary/test that transitively links
// `net_pcap.rs` (PcapNtpTransport / NpcapPtpNetwork) -- so the OS loader
// refuses to even START the process on a runtime-less runner
// (STATUS_DLL_NOT_FOUND / 0xc0000135, confirmed live in release run
// 30336428951), killing test EXECUTION entirely rather than failing just
// the pcap-touching tests.
//
// `/DELAYLOAD:wpcap.dll` defers the loader's resolution of the DLL to the
// FIRST actual call into it -- necessary but NOT sufficient on its own:
// net_pcap.rs's OWN #[test]s (test_pcap_ts_to_systemtime, test_ptp_constants,
// etc.) are pure arithmetic/logic and never call Device::list()/
// Capture::from_device(), but ntp.rs's pre-existing `test_ntp_client_new`
// DOES reach Device::list() indirectly (NtpClient::new() ->
// PcapNtpTransport::new() -> find_device()) -- and MSVC's default delay-load
// failure hook raises an UNRECOVERABLE structured exception the moment a
// delay-loaded symbol is first called and the DLL can't be found, so
// delay-load alone still crashed that test (0xc06d007e, confirmed live).
// The actual safety net is net_pcap.rs's own `wpcap_runtime_available()`
// guard (probes via statically-linked kernel32 LoadLibraryW/FreeLibrary,
// never delay-loaded), called at the top of `find_device()` -- the shared
// choke point for both capture paths -- BEFORE the first real pcap:: call.
// That is what turns a missing runtime into a normal `Err` everywhere,
// including from code this build.rs change alone cannot see into. A future
// pcap:: call site that bypasses that guard will crash the same way this
// one did -- route every new pcap:: entry point through find_device() (or
// call wpcap_runtime_available() directly) rather than relying on
// delay-load by itself.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:wpcap.dll");
        // MSVC's delay-load helper thunks (__delayLoadHelper2 etc.) live in
        // delayimp.lib, which is NOT linked automatically just because
        // /DELAYLOAD is passed -- it must be supplied explicitly.
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
