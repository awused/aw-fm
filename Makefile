default:
	@grep '^[^#[:space:]].*:' Makefile

.NOTPARALLEL:

install:
	cargo install --locked --path .
	cargo install --locked --path xdg-desktop-portal-aw-fm
	mkdir -p "${HOME}/.local/share/applications"
	mkdir -p "${HOME}/.local/share/dbus-1/services"
	mkdir -p "${HOME}/.local/share/xdg-desktop-portal/portals"
	cp desktop/aw-fm.desktop "${HOME}/.local/share/applications/aw-fm.desktop"
	cp desktop/aw-fm-folder.desktop "${HOME}/.local/share/applications/aw-fm-folder.desktop"
	cp desktop/org.aw-fm.freedesktop.FileManager1.service "${HOME}/.local/share/dbus-1/services/org.aw-fm.freedesktop.FileManager1.service"
	cp desktop/aw-fm.portal "${HOME}/.local/share/xdg-desktop-portal/portals/aw-fm.portal"

uninstall:
	cargo uninstall aw-fm
	cargo uninstall xdg-desktop-portal-aw-fm
	rm "${HOME}/.local/share/applications/aw-fm.desktop"
	rm "${HOME}/.local/share/applications/aw-fm-folder.desktop"
	rm "${HOME}/.local/share/dbus-1/services/org.aw-fm.freedesktop.FileManager1.service"
	rm "${HOME}/.local/share/xdg-desktop-portal/portals/aw-fm.portal"

