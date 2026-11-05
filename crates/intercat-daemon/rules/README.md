# Gitleaks rules

`gitleaks-8.30.1.toml` is the complete default ruleset from
<https://raw.githubusercontent.com/gitleaks/gitleaks/v8.30.1/config/gitleaks.toml>.
It is compiled into the daemon; project configuration cannot replace it.

Upstream SHA-256: `e163e53b9e7e8a8511e77271e2b323ed057759542a6d988258afe3a1fa329caf`.

The only change groups the global boolean/null allowlist alternatives:
`(?i)^true|false|null$` becomes `(?i)^(?:true|false|null)$`.
Otherwise a real token containing `false` or ending in `null` can be skipped.
The issue and reproduction are at <https://github.com/gitleaks/gitleaks/issues/2293>.
The full file is necessary because extending the default configuration appends
global allowlists and cannot remove this unsafe default expression.

Local SHA-256: `9ad910b401b67eba450292914a458152b1d75b26d85e59964ef6992fe7dc9db5`.
The upstream MIT license is retained in `LICENSE.gitleaks`.

When changing this file, keep its source and patch explicit. The ruleset digest
is part of scan bindings and Check settings, so previous checks must be rebuilt.

Repair previews also compile all 221 text regexes from this same file with
`pcre2-sys` 0.2.10 (bundled PCRE2 10.46, statically linked). They use
[DFA hard partial matching](https://www.pcre2.org/doc/pcre2partial/), retaining
the whole bounded item so multiline matches and preceding context remain valid.
Entropy thresholds, keyword prefilters, and allowlists do not relax this live
preview check. Complete items still pass through Gitleaks before recording.
The path-only rule is relevant to file scans, not message text.

The preview worker holds text when compilation/matching fails, a resource limit
is reached, or a possible credential is incomplete. Raw fragments stay in
bounded process memory and never enter the event log. Known HMAC fingerprints
and provider spans are merged on original byte offsets before replacement.
