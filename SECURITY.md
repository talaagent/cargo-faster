# Security policy

Security fixes target the latest published version. Report vulnerabilities
privately through [GitHub private vulnerability reporting](https://github.com/talaagent/cargo-faster/security/advisories/new).
If that form is unavailable, email **github@igm.pub**. Please do not disclose
an unpatched vulnerability in a public issue.

Include the version, OS, impact, and a minimal reproduction. cargo faster
runs cargo and rustc, reads cargo configuration, environment variables,
`/proc` and the output of tools such as `kache` and `df`, and prints parts of
them. Relevant issues include a command it runs that differs from what it
reports, arguments or environment from a project's configuration that
escape into a command they shouldn't reach, secrets from the environment or
a rustc command line printed in a report, writes outside the target and
temporary directories, and hangs or unbounded memory on a hostile profile,
timings file or tool output. Remove secrets from command lines, environment
dumps and reports before sharing them.
