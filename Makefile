default:
	@grep '^[^#[:space:]].*:' Makefile

.NOTPARALLEL:

install:
	cargo install --locked --path .
	mkdir -p "${HOME}/.local/share/applications/"
	mkdir -p "${HOME}/.local/share/dbus-1/services"
	cp desktop/aw-fm.desktop "${HOME}/.local/share/applications/aw-fm.desktop"
	cp desktop/aw-fm-folder.desktop "${HOME}/.local/share/applications/aw-fm-folder.desktop"
	cp desktop/org.aw-fm.freedesktop.FileManager1.service "${HOME}/.local/share/dbus-1/services/org.aw-fm.freedesktop.FileManager1.service"

uninstall:
	cargo uninstall aw-fm
	rm "${HOME}/.local/share/applications/aw-fm.desktop"
	rm "${HOME}/.local/share/applications/aw-fm-folder.desktop"
	rm "${HOME}/.local/share/dbus-1/services/org.aw-fm.freedesktop.FileManager1.service"

