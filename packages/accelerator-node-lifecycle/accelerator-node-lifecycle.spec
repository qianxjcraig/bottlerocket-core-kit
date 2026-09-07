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

Source1: accelerator-node-lifecycle-resume.service
Source2: accelerator-node-lifecycle-resume.path
Source3: accelerator-node-lifecycle-tmpfiles.conf

BuildRequires: %{_cross_os}glibc-devel

%description
Persists and validates resumable NVIDIA accelerator profile transitions for the
node-side lifecycle component while keeping cluster operations separately owned.
It also provides an allowlisted NVIDIA provider reconciler and a boot-enabled
service that resumes persisted node-owned actions after interruption.

%prep
%setup -T -c
%cargo_prep

%build
%cargo_build --manifest-path %{_builddir}/sources/Cargo.toml \
    -p accelerator-node-lifecycle

%install
install -d %{buildroot}%{_cross_bindir}
install -p -m 0755 %{__cargo_outdir}/accelerator-node-lifecycle %{buildroot}%{_cross_bindir}
install -p -m 0755 %{__cargo_outdir}/accelerator-node-lifecycle-resume %{buildroot}%{_cross_bindir}
install -p -m 0755 %{__cargo_outdir}/nvidia-provider-reconciler %{buildroot}%{_cross_bindir}

install -d %{buildroot}%{_cross_unitdir}
install -p -m 0644 %{S:1} %{S:2} %{buildroot}%{_cross_unitdir}

install -d %{buildroot}%{_cross_unitdir}/multi-user.target.wants
ln -s ../accelerator-node-lifecycle-resume.service \
    %{buildroot}%{_cross_unitdir}/multi-user.target.wants/accelerator-node-lifecycle-resume.service
ln -s ../accelerator-node-lifecycle-resume.path \
    %{buildroot}%{_cross_unitdir}/multi-user.target.wants/accelerator-node-lifecycle-resume.path

install -d %{buildroot}%{_cross_tmpfilesdir}
install -p -m 0644 %{S:3} %{buildroot}%{_cross_tmpfilesdir}/accelerator-node-lifecycle.conf

%files
%{_cross_bindir}/accelerator-node-lifecycle
%{_cross_bindir}/accelerator-node-lifecycle-resume
%{_cross_bindir}/nvidia-provider-reconciler
%{_cross_unitdir}/accelerator-node-lifecycle-resume.service
%{_cross_unitdir}/accelerator-node-lifecycle-resume.path
%{_cross_unitdir}/multi-user.target.wants/accelerator-node-lifecycle-resume.service
%{_cross_unitdir}/multi-user.target.wants/accelerator-node-lifecycle-resume.path
%{_cross_tmpfilesdir}/accelerator-node-lifecycle.conf
