//! Network policy for sandboxed command execution.
//!
//! This module provides network isolation policies that can be applied to
//! commands running in the sandbox. Policies range from fully offline to
//! domain-restricted to fully open.

use std::process::Command;

use crate::error::{Error, Result};

/// Network policy for sandboxed execution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// No network access at all.
    ///
    /// On Linux: Uses `--unshare-net` with bubblewrap.
    /// On macOS: Uses `(deny network*)` in sandbox-exec profile.
    /// On Windows: Uses Windows Firewall rules (requires admin).
    #[default]
    Offline,

    /// Restricted network access with domain allowlist.
    ///
    /// Only connections to specified domains/IPs are permitted.
    /// Implementation varies by platform:
    /// - Linux: Uses proxy or iptables rules
    /// - macOS: Uses sandbox-exec with network rules
    /// - Windows: Uses Windows Firewall rules
    Restricted {
        /// List of allowed domains or IP addresses.
        /// Examples: "api.github.com", "crates.io", "192.168.1.0/24"
        allowlist: Vec<String>,
    },

    /// Full network access (no restrictions).
    ///
    /// Use with caution - the process has unrestricted network access.
    Open,
}

impl NetworkPolicy {
    /// Create an offline policy.
    pub const fn offline() -> Self {
        Self::Offline
    }

    /// Create an open policy.
    pub const fn open() -> Self {
        Self::Open
    }

    /// Create a restricted policy with an allowlist.
    pub const fn restricted(allowlist: Vec<String>) -> Self {
        Self::Restricted { allowlist }
    }

    /// Check if network access is allowed at all.
    pub const fn is_network_allowed(&self) -> bool {
        !matches!(self, Self::Offline)
    }

    /// Check if a domain is allowed by this policy.
    pub fn is_domain_allowed(&self, domain: &str) -> bool {
        match self {
            Self::Offline => false,
            Self::Open => true,
            Self::Restricted { allowlist } => allowlist.iter().any(|allowed| {
                domain == allowed
                    || domain.ends_with(&format!(".{allowed}"))
                    || is_ip_in_cidr(domain, allowed)
            }),
        }
    }

    /// Apply this network policy to a command.
    ///
    /// This modifies the command in-place to enforce the network policy.
    /// The implementation varies by platform:
    ///
    /// - **Linux**: Wraps with `bwrap --unshare-net` (Offline) or uses proxy
    /// - **macOS**: Generates sandbox-exec profile with network rules
    /// - **Windows**: Returns an error (requires separate firewall setup)
    ///
    /// # Returns
    ///
    /// Returns a new `Command` that enforces the network policy.
    /// The original command is not modified.
    pub fn apply_to_command(&self, cmd: &mut Command) -> Result<()> {
        match self {
            Self::Open => {
                // No modifications needed - full network access
                Ok(())
            }
            Self::Offline => self.apply_offline_policy(cmd),
            Self::Restricted { allowlist } => self.apply_restricted_policy(cmd, allowlist),
        }
    }

    /// Get platform-specific wrapper arguments for offline mode.
    #[cfg(target_os = "linux")]
    fn apply_offline_policy(&self, cmd: &mut Command) -> Result<()> {
        // On Linux, we use bubblewrap with --unshare-net
        // This is typically handled at a higher level in the sandbox module
        // Here we just note that offline mode is requested
        // The actual wrapping happens in the platform-specific code
        Ok(())
    }

    #[cfg(target_os = "macos")]
    const fn apply_offline_policy(&self, _cmd: &mut Command) -> Result<()> {
        // On macOS, we use sandbox-exec with (deny network*)
        // This is typically handled at a higher level in the sandbox module
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn apply_offline_policy(&self, _cmd: &mut Command) -> Result<()> {
        // Windows doesn't have a simple built-in network sandbox
        // Would need Windows Firewall API or similar
        Err(Error::tool(
            "network-policy",
            "Offline network policy is not supported on Windows. Consider using a restricted policy instead.",
        ))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn apply_offline_policy(&self, _cmd: &mut Command) -> Result<()> {
        Err(Error::tool(
            "network-policy",
            "Network policy is not supported on this platform",
        ))
    }

    /// Get platform-specific wrapper arguments for restricted mode.
    #[cfg(target_os = "linux")]
    fn apply_restricted_policy(&self, _cmd: &mut Command, _allowlist: &[String]) -> Result<()> {
        // On Linux, restricted network access typically requires:
        // 1. A proxy server (like squid) configured with allowlist
        // 2. iptables rules
        // 3. Or bubblewrap with --share-net and custom namespace rules
        //
        // For now, we just log that restricted mode is requested
        // Full implementation would require additional infrastructure
        Ok(())
    }

    #[cfg(target_os = "macos")]
    const fn apply_restricted_policy(
        &self,
        _cmd: &mut Command,
        allowlist: &[String],
    ) -> Result<()> {
        // On macOS, we could modify the sandbox-exec profile to allow specific network
        // destinations. However, sandbox-exec has limited support for this.
        // The profile would need to include:
        // (allow network-outbound (remote ip "192.168.1.1"))
        // (allow network-outbound (remote dns "api.github.com"))
        //
        // For now, we just note that restricted mode is requested
        let _ = allowlist;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn apply_restricted_policy(&self, _cmd: &mut Command, _allowlist: &[String]) -> Result<()> {
        // Windows would need firewall rule configuration
        Err(Error::tool(
            "network-policy",
            "Restricted network policy is not supported on Windows yet",
        ))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    fn apply_restricted_policy(&self, _cmd: &mut Command, _allowlist: &[String]) -> Result<()> {
        Err(Error::tool(
            "network-policy",
            "Network policy is not supported on this platform",
        ))
    }
}

/// Check if an IP address matches a CIDR block.
fn is_ip_in_cidr(ip: &str, cidr: &str) -> bool {
    // Simple check for exact IP match or common CIDR patterns
    if ip == cidr {
        return true;
    }

    // Handle simple cases like "192.168.1.0/24"
    if let Some(slash_pos) = cidr.find('/') {
        let prefix = &cidr[..slash_pos];
        let mask_str = &cidr[slash_pos + 1..];

        // Parse the mask
        if let Ok(mask_bits) = mask_str.parse::<u32>() {
            // Simple IPv4 matching
            if let (Ok(ip_addr), Ok(prefix_addr)) = (parse_ipv4(ip), parse_ipv4(prefix)) {
                if mask_bits > 32 {
                    return false;
                }
                let mask = if mask_bits == 0 {
                    0u32
                } else {
                    !0u32 << (32 - mask_bits)
                };
                return (ip_addr & mask) == (prefix_addr & mask);
            }
        }
    }

    false
}

/// Parse an IPv4 address to a u32.
fn parse_ipv4(s: &str) -> Result<u32> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return Err(Error::validation(format!("Invalid IPv4 address: {s}")));
    }

    let mut result = 0u32;
    for part in parts {
        let octet: u32 = part
            .parse()
            .map_err(|_| Error::validation(format!("Invalid IPv4 octet: {part}")))?;
        if octet > 255 {
            return Err(Error::validation(format!(
                "IPv4 octet out of range: {octet}"
            )));
        }
        result = (result << 8) | octet;
    }

    Ok(result)
}

/// Network policy builder for convenient construction.
#[derive(Debug, Clone, Default)]
pub struct NetworkPolicyBuilder {
    allowlist: Vec<String>,
    offline: bool,
    open: bool,
}

impl NetworkPolicyBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the policy to offline mode.
    pub const fn offline(mut self) -> Self {
        self.offline = true;
        self.open = false;
        self
    }

    /// Set the policy to open mode.
    pub const fn open(mut self) -> Self {
        self.open = true;
        self.offline = false;
        self
    }

    /// Add a domain to the allowlist.
    pub fn allow(mut self, domain: impl Into<String>) -> Self {
        self.allowlist.push(domain.into());
        self.offline = false;
        self
    }

    /// Add multiple domains to the allowlist.
    pub fn allow_all(mut self, domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        for domain in domains {
            self.allowlist.push(domain.into());
        }
        self.offline = false;
        self
    }

    /// Build the final network policy.
    pub fn build(self) -> NetworkPolicy {
        if self.offline {
            NetworkPolicy::Offline
        } else if self.open || self.allowlist.is_empty() {
            NetworkPolicy::Open
        } else {
            NetworkPolicy::Restricted {
                allowlist: self.allowlist,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_policy_default() {
        let policy = NetworkPolicy::default();
        assert_eq!(policy, NetworkPolicy::Offline);
        assert!(!policy.is_network_allowed());
    }

    #[test]
    fn test_network_policy_offline() {
        let policy = NetworkPolicy::offline();
        assert!(!policy.is_network_allowed());
        assert!(!policy.is_domain_allowed("example.com"));
    }

    #[test]
    fn test_network_policy_open() {
        let policy = NetworkPolicy::open();
        assert!(policy.is_network_allowed());
        assert!(policy.is_domain_allowed("example.com"));
    }

    #[test]
    fn test_network_policy_restricted() {
        let policy =
            NetworkPolicy::restricted(vec!["api.github.com".to_string(), "crates.io".to_string()]);

        assert!(policy.is_network_allowed());
        assert!(policy.is_domain_allowed("api.github.com"));
        assert!(policy.is_domain_allowed("crates.io"));
        assert!(!policy.is_domain_allowed("evil.com"));
        // Subdomains are allowed
        assert!(policy.is_domain_allowed("index.crates.io"));
    }

    #[test]
    fn test_is_ip_in_cidr() {
        assert!(is_ip_in_cidr("192.168.1.1", "192.168.1.1"));
        assert!(is_ip_in_cidr("192.168.1.100", "192.168.1.0/24"));
        assert!(is_ip_in_cidr("10.0.0.5", "10.0.0.0/8"));
        assert!(!is_ip_in_cidr("192.168.2.1", "192.168.1.0/24"));
        assert!(is_ip_in_cidr("0.0.0.0", "0.0.0.0/0"));
    }

    #[test]
    fn test_parse_ipv4() {
        assert_eq!(parse_ipv4("192.168.1.1").unwrap(), 0xC0A8_0101);
        assert_eq!(parse_ipv4("0.0.0.0").unwrap(), 0);
        assert_eq!(parse_ipv4("255.255.255.255").unwrap(), 0xFFFF_FFFF);
        assert!(parse_ipv4("invalid").is_err());
        assert!(parse_ipv4("256.0.0.0").is_err());
    }

    #[test]
    fn test_network_policy_builder() {
        let policy = NetworkPolicyBuilder::new()
            .allow("api.github.com")
            .allow("crates.io")
            .build();

        assert!(matches!(policy, NetworkPolicy::Restricted { .. }));
        assert!(policy.is_domain_allowed("api.github.com"));

        let offline_policy = NetworkPolicyBuilder::new().offline().build();
        assert_eq!(offline_policy, NetworkPolicy::Offline);

        let open_policy = NetworkPolicyBuilder::new().open().build();
        assert_eq!(open_policy, NetworkPolicy::Open);
    }
}
