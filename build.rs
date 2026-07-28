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
// FIRST actual call into it. Verified safe: every #[test] reachable from
// net_pcap.rs (test_pcap_ts_to_systemtime, test_ptp_constants, etc.) is pure
// arithmetic/logic -- none of them call Device::list()/Capture::from_device()
// or any other real pcap:: API; those are only reached from
// NpcapPtpNetwork::new()/PcapNtpTransport::new(), production code paths a
// test never exercises. So with delay-load, every existing test genuinely
// runs to completion without ever touching wpcap.dll. A future test that DOES
// call into pcap on a runtime-less runner will fail loudly at that call --
// an honest, real missing-capability failure, never a silent no-op.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:wpcap.dll");
        // MSVC's delay-load helper thunks (__delayLoadHelper2 etc.) live in
        // delayimp.lib, which is NOT linked automatically just because
        // /DELAYLOAD is passed -- it must be supplied explicitly.
        println!("cargo:rustc-link-arg=delayimp.lib");
    }
}
