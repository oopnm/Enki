%global debug_package %{nil}

Name:           enki
Version:        0.1.0
Release:        1%{?dist}
Summary:        Use your phone as a webcam, microphone and screen source

License:        GPL-3.0-or-later
URL:            https://github.com/oopnm/PhoneCam-Studio
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo rust gcc gtk4-devel pkgconfig
Requires:       gtk4 v4l-utils polkit
# غير متوفرة في مستودعات Fedora الرسمية (RPM Fusion / COPR)
Recommends:     scrcpy akmod-v4l2loopback

%description
Enki turns an Android phone into a Linux webcam (via v4l2loopback), and
streams the phone screen using scrcpy.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --locked -p enki-gui

%install
install -Dm755 target/release/enki %{buildroot}%{_bindir}/enki
install -Dm644 packaging/appimage/enki.desktop %{buildroot}%{_datadir}/applications/enki.desktop
install -Dm644 packaging/appimage/enki.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/enki.png

%files
%license LICENSE
%{_bindir}/enki
%{_datadir}/applications/enki.desktop
%{_datadir}/icons/hicolor/256x256/apps/enki.png

%changelog
* Fri Nov 13 2026 Ahmed Al-Iraqi <ahmed@local> - 0.1.0-1
- Initial release
