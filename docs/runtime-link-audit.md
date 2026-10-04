# Historical runtime audit

The runtime audit through checkpoint `ece05bd` used an exact MIT-or-Apache-2.0 allowlist. On 2026-10-05, the project policy changed to dependencies compatible with MIT and Apache-2.0, including ordinary permissive runtime and Unicode licenses. The current policy and distribution requirements are in [licensing.md](licensing.md).

The GNU entry adapter, OpenSSL configurations, linker/source/data inspectors and their C and unsafe Rust fixtures have been removed from the project. They are unnecessary for the corrected policy. Historical implementations and documentation can be inspected at [checkpoint ece05bd](https://github.com/peppermintish/puffinbox/tree/ece05bdfca3566e4d6d241653d238446d5fc76c8). Private evidence under `.local/` has been preserved.

Those experiments produced scoped source inventories, notice comparisons, numerical controls and container/client observations. They did not establish complete binary provenance, full feature acceptance or a release. Their old exact-license blockers are historical and must not be applied as current requirements.

Production continues to use the static Linux build. Its current acceptance requires a rustls dependency graph, full notices and source/container/client checks against the rebuilt artifact. Older image identities and results remain scoped to their tested source in [acceptance.md](acceptance.md).
