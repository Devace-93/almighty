# Headless desktop for driving the Linux desktop client in tests:
# Xvfb + the WebKitGTK runtime + xdotool (input) + ImageMagick (screenshots).
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      xvfb xdotool imagemagick dbus-x11 ca-certificates \
      libwebkit2gtk-4.1-0 libgtk-3-0 libxdo3 libayatana-appindicator3-1 librsvg2-2 \
      fonts-dejavu-core fonts-liberation gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
    && rm -rf /var/lib/apt/lists/*
ENV DISPLAY=:99 GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 NO_AT_BRIDGE=1
COPY entry.sh /entry.sh
ENTRYPOINT ["/entry.sh"]
