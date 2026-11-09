# Third-party components

Versions are fixed by `Cargo.toml` and `Cargo.lock`.

| Component | Locked version | License | Purpose |
| --- | ---: | --- | --- |
| Microsoft `windows-sys` | 0.59.0 | MIT OR Apache-2.0 | Raw Win32 API bindings |
| Microsoft `windows-targets` | 0.52.6 | MIT OR Apache-2.0 | Target-specific import libraries used by `windows-sys` |

The target-specific `windows_x86_64_msvc` package is also locked at 0.52.6 and
uses the same MIT OR Apache-2.0 licensing. These permissive licenses are
compatible with distribution of this GPL-2.0-or-later program. No proprietary
runtime framework is bundled; the executable calls standard Windows system
DLLs.
