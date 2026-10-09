# Robust grandmaster: Dante GM failover + a dantesync fallback time source (issue 129)

Status: DRAFT for owner review, 8.10.2026.

## Goal

Keep every dantesync node in ONE PTP-grade time domain when:
- (a) the main Dante grandmaster disappears while another Dante clock leader exists;
- (b) no Dante PTP exists on the network at all.

It must never disturb a real Dante network.

The owner's words (8.10.2026):

> "...ak nie je pritomny hlavny grandmaster tak sa prepne na fallback a aj v situaciach ked dante ptp nie je k dispozicii tak by sa mal aktivovat nejaky fallback mod kedy jeden z dantesyncov sa stane ptp grandamasterom a ostatne dantesynci sa zacnu syncovat voci nemu a clock sice nebude dokonale presny ale bude to podstatne lepsie ako ziaden genlock"

The trigger: Poprad's resolume-pp reads `mode NTP-only`, `gm_source_ip none`, so every PP genlock output is UNLOCKED (camera-box #1361).

## What exists today (origin/master a991f13, v1.16.0)

**Receive and timestamps**
- PTP is receive only: PTPv1 (Dante), two-step Sync + Follow_Up on 224.0.1.129, ports 319/320.
- Timestamps are software: Linux `SO_TIMESTAMPNS`, Windows Npcap host timestamps.
- Nothing ever transmits PTP.

**Grandmaster choice**
- There is no grandmaster selection: the last allowed sender wins.
- `system.gm_allowlist` pins the fleet to one GM: `video-clock.lan`, or the audio GM on the audio VLAN.
- With no allowed packet for 10 s (`ptp_stale_at`) a node goes `NTP-only`. It holds its learned frequency, and NTP steps correct the phase (the "sawtooth").

**A parse bug to fix first**
- `PtpV1Header::SIZE` is 36. The PTPv1 header is 40 bytes.
- The Sync body parser skips 13 bytes instead of 14: it misses the reserved byte before `grandmasterCommunicationTechnology`.
- So `gm_uuid` is read 5 bytes early and reads the constant `[0,0,0,0,1,0]` on every live node.
- As a result, "GRANDMASTER UUID CHANGED" and the date authority's GM check cannot tell grandmasters apart.
- The best-master fields (stratum, identifier, variance, preferred) are not parsed.

**Receive filtering:** the PTP version and subdomain are not checked, and a node's own packets would not be filtered.

**The date master:**
- the static NTP master (`ntp_server_mode`, strih-lx at SNV);
- followers poll it on UDP 31900;
- the date offset lives on the Dante time base.

## Approaches for (b), the fallback time source

**Approach 1 (recommended): a dantesync-private time distribution.** Only dantesync nodes follow it.
- The elected node multicasts two-step Sync + Follow_Up messages of a dantesync format on its OWN group and port (proposed `239.255.77.1:31910`). It never uses 224.0.1.129 or ports 319/320.
- Dante devices never see it as PTP, so it can never win a Dante leader election. It does not fight Dante Virtual Soundcard for 319/320 on Windows. A follower can never mistake it for Dante time.
- Dante audio devices themselves keep their own clock behaviour.

**Approach 2 (rejected): transmit Dante-format PTPv1 on 224.0.1.129, so Dante devices follow too.**
- The node would take part in Dante's own leader election. A software-timestamped clock could take over a real Dante network, which is the opposite of safe.
- It collides with DVS's `ptp.exe` on 319/320.
- Every receiver would need to tell dantesync PTP apart from Dante PTP.

**Approach 3 (kept as the last resort, already exists): NTP-disciplined frequency.**
- A PI loop on the existing 1 Hz NTP / 31900 offsets.
- It needs no new protocol, but it is coarse: unicast at 1 Hz, and the Windows NTP scatter is ±20 ms (#53).
- Today's NTP-only stays the floor when no dantesync source answers either.

## The design (Approach 1)

### Slice 0: correctness first (no behaviour change for the fleet)

**Parse fix**
- Header size 40, correct Sync body offsets.
- Parse `grandmasterClockUuid`, stratum, identifier, variance and preferred.
- `/status.gm_uuid` becomes the real GM identity.

**Receive filter**
- Accept only PTP version 1.
- Accept only the Dante subdomain the live GMs use: read off the wire first, recorded in the spec before enforcing.
- Drop packets whose source UUID is the node's own.

**Read off the wire (9.10.2026, dev1 enp2s0, video VLAN, `tests/fixtures/ptpv1/`):**
- versionPTP 1, versionNetwork 1, subdomain `_DFLT`;
- grandmaster `10.77.9.230`: UUID `00:1d:c1:08:02:14` (= the Sync's sourceUuid, port 2), stratum
  0x79 (121), identifier `DFLT`, variance −4000, preferred 1, boundary clock 1, syncInterval −2.
- The audio VLAN is not read yet: the 1.17 `/status` fields (`gm_uuid`, `ptp_subdomain`,
  `ptp_source_uuid`) record it after the rollout.

**Report-only rollout**
- The date authority's GM check starts to discriminate once `gm_uuid` is real. Before it does, deploy in report mode and log the GM UUID every node sees.
- Then check that the video-VLAN and audio-VLAN nodes see one AIC128-D clock identity, or record how they differ. Only then enforce.
- This slice is independently shippable.

### Slice 1: Dante GM failover

**Config.** `system.gm_preference` is an ordered list.
- Entries take the same forms as the allowlist: host, IP, CIDR.
- The default is today's allowlist as a one-entry list, so behaviour is byte-identical for the current fleet.

**Selection** is pure, in `gm_filter.rs`. It uses `ptp_stale_at`, never a second timeout (`.claude/rules/ptp-liveness-rejoin.md`).
- Follow the highest-preference GM that is live.
- Leave the current GM only when it goes stale (10 s).
- Return to a higher-preference GM only after it has been continuously live for `gm_return_hold_s` (default 60 s).

**A switch** is the existing soft reset:
- keep the learned frequency;
- re-anchor `D`;
- no wall step;
- the phase lock follows the GM's frequency step (the v1.14 detector).

**Dante's own re-election** (another Dante device becomes leader) is followed when that device is in the preference list. At SNV the list could be the console card's two ports, then the venue's Dante subnet. The order is an owner/venue fact per site.

### Slice 2: the fallback time source

**Roles**
- Each node has `fallback.priority`: 0 means never a source.
- The fleet config gives the date master (strih-lx at SNV, the PP strih at Poprad) the highest priority. The time base and the date authority then stay on one box.
- Other always-on nodes get lower priorities: dev1, then stream.
- Cameras are 0.

**Election**
- A candidate starts as the fallback source only when BOTH hold:
  - it has had no live Dante PTP for `ptp_stale_at`;
  - it has heard no higher-priority announcement for `fallback.yield_s`, a backoff scaled by its rank (default 3 s × rank).
- Followers lock to the highest-priority live announcer.
- The source's loss is the same staleness rule; the next priority takes over.
- One source at a time is guaranteed by the priority order. Equal priorities are a config error, refused at load.

**The source's own clock**
- It runs on HOLDOVER: the last Dante-disciplined frequency, phase lock frozen.
- It takes no NTP steps while it is the source.
- The daily date correction still keeps UTC within its bound.

**Messages**
- Sync at 8 Hz, plus a Follow_Up carrying the precise send time.
- The send timestamp on Windows comes from the Npcap capture of the node's own frame, the NTP transport's existing primitive.
- On Linux it is the kernel software TX timestamp (`SO_TIMESTAMPING` + `MSG_ERRQUEUE`).
- Messages carry: a magic, a version, the source's priority and node identity, an epoch (it changes when a new source takes over), and the sequence.

**Followers**
- The samples feed the SAME rate servo and phase lock as PTP. Followers use the same servo, never a second one.
- New `/status` values (additive):
  - `mode` `FALLBACK`;
  - `gm_class` `dante` | `dantesync-fallback` | `none`;
  - `fallback_role` `source` | `follower` | `candidate` | `off`;
  - `fallback_source_ip`;
  - `fallback_epoch`.
- `is_locked` keeps its meaning (rate locked). A fallback lock always reads `mode FALLBACK`, never `LOCK` or `NANO`, so no consumer can take it for a Dante lock.

**Hand-back to Dante**
- When Dante PTP is live again continuously for `fallback.dante_return_hold_s` (default 60 s), the source stops announcing.
- Followers return to Dante through the slice-1 soft re-anchor: no wall step, and the frequency difference is slewed by the phase lock.

**Expected accuracy**
- Software timestamps at both ends; one-way LAN latency is not measured (#42 tracks two-way delay).
- So: tens of µs of jitter plus a constant per-path offset of roughly 0.1–0.5 ms.
- That is worse than a hardware Dante leader, and far better than NTP-only's ms-scale sawtooth. One genlock frame is 16.7 ms at 60 fps.

### Slice 3: contracts

**Journal:** a new `[FALLBACK]` line family, mutually non-substring with `[PTP]` / `[NTP]`.

**camera-box** (its own ticket there):
- The clock-offset gate, the dante-clock watchdog and the in-OBS LOCK indicator learn `mode FALLBACK` / `gm_class`.
- A fallback lock reads DEGRADED-but-genlocked, never LOCK and never UNLOCKED.

**Canonical config:** `scripts/dantesync-canonical-config.json` gains the per-role priorities and the preference lists.

## Testing

- **Slice 0:** unit tests on captured real Dante Sync bytes. The live GM UUID and the best-master fields round-trip, the version/subdomain filter rejects PTPv2 and foreign subdomains, and the self-filter works.
- **Slices 1 and 2:** the existing `tests/two_clock_bench` world plus `tests/simulation_e2e.rs`. Scenarios:
  - primary GM loss with a backup Dante GM present;
  - all Dante PTP gone: a source is elected, followers lock within a bound, and the cross-box spread stays under a bound over 8 h;
  - source loss, then the next priority takes over;
  - Dante returns, then the hand-back with no wall step;
  - two candidates with a partition healing: one source wins.
- **The never-on-Dante pin:** a test asserts no code path sends to 224.0.1.129 or to ports 319/320.
- **Rig acceptance** (supervisor step, owner-visible), at SNV in TEST mode:
  - block the console card's PTP at the switch;
  - the fleet goes FALLBACK with strih-lx as source;
  - camera-box's genlock stays grid-aligned;
  - unblock: back to the Dante lock with no wall step.

## Rollout

- Slice 0 first, report-only, then enforced.
- Slices 1 and 2 behind config: the default is off, so the old behaviour is unchanged until the canonical config enables them.
- Canary per OS class (the existing fleet-upgrade path), then the fleet.
- PP gets the same config with its own Dante device first in its preference list.

## Out of scope

- Hardware timestamping.
- Two-way delay correction (#42).
- Making Dante audio devices follow dantesync (Approach 2, rejected above).
