%global _cross_first_party 1
%undefine _debugsource_packages
%global cross_generate_sbom %{shrink: \
  mkdir -p %{_builddir}/sbom-temp && \
  sbomtool generate \
    --name accelerator-node-lifecycle \
    --out-dir %{_builddir}/sbom-temp \
    --build-dir %{_builddir}/sources \
    --spdx --cyclonedx}

Name: %{_cross_os}accelerator-node-lifecycle
Version: 0.1.0
Release: 1%{?dist}
Epoch: 1
Summary: Durable accelerator profile transition utility for Bottlerocket nodes
License: Apache-2.0 OR MIT
URL: https://github.com/bottlerocket-os/bottlerocket-core-kit

BuildRequires: %{_cross_os}glibc-devel

%description
Persists and validates resumable NVIDIA accelerator profile transitions for the
node-side lifecycle component while keeping cluster operations separately owned.

%prep
%setup -T -c
%cargo_prep

%build
%cargo_build --manifest-path %{_builddir}/sources/Cargo.toml \
    -p accelerator-node-lifecycle

%install
install -d %{buildroot}%{_cross_bindir}
install -p -m 0755 %{__cargo_outdir}/accelerator-node-lifecycle %{buildroot}%{_cross_bindir}

%files
%{_cross_bindir}/accelerator-node-lifecycle
