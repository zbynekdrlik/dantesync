//! dantesync#129 (slice 0) — who sends the PTP time this node follows, as the Sync carries it.
//!
//! Before 1.17 the header was parsed as 36 bytes, so every node read the grandmaster UUID as the
//! constant `ptp::LEGACY_MISREAD_GM_UUID` and never read the version or the subdomain. Now the
//! header's identity (versionPTP, subdomain, sourceUuid) is logged on a change and published on
//! `/status` (`ptp_version`, `ptp_subdomain`, `ptp_source_uuid`), and the grandmaster's
//! best-master fields join its log line. All of it is REPORT-ONLY: a foreign version or subdomain
//! is followed exactly as before, until the live identities of both VLANs are recorded on
//! issue 129 and the filter is enforced in a later commit.

use super::*;

/// The identity in a Sync's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PtpSender {
    pub(super) version: u16,
    pub(super) subdomain: String,
    pub(super) source_uuid: [u8; 6],
}

/// The best-master fields of a Sync's grandmaster block, for its log line.
pub(super) fn describe_grandmaster(body: &PtpV1SyncMessageBody) -> String {
    format!(
        "stratum {}, identifier \"{}\", variance {}, preferred {}",
        body.grandmaster_clock_stratum,
        body.grandmaster_identifier_name(),
        body.grandmaster_clock_variance,
        if body.grandmaster_preferred {
            "yes"
        } else {
            "no"
        }
    )
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// Note the Sync header's identity: logged when it changes, published on `/status`.
    pub(super) fn note_ptp_sender(&mut self, header: &PtpV1Header) {
        let sender = PtpSender {
            version: header.version_ptp,
            subdomain: header.subdomain_name(),
            source_uuid: header.source_uuid,
        };
        if self.current_ptp_sender.as_ref() == Some(&sender) {
            return;
        }
        info!(
            "PTP sender: PTPv{} (network v{}), subdomain \"{}\", source {} port {} (reported \
             only, issue 129)",
            sender.version,
            header.version_network,
            sender.subdomain,
            format_mac(&sender.source_uuid),
            header.source_port_id
        );
        self.current_ptp_sender = Some(sender);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grandmaster_line_names_its_best_master_fields_129() {
        let sync = include_bytes!("../../tests/fixtures/ptpv1/dante-sync.bin");
        let body = PtpV1SyncMessageBody::parse(&sync[PtpV1Header::SIZE..]).expect("a Sync body");
        assert_eq!(
            describe_grandmaster(&body),
            "stratum 121, identifier \"DFLT\", variance -4000, preferred yes"
        );
    }
}
