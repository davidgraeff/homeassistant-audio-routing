# Throwaway image for spike 3 — adds Python + aiosendspin on top of the
# real pw-audio-router image, so the spike runs against the exact same
# base the production add-on will use. Not part of the production
# Dockerfile: bridge-daemon packaging is a later decision (PLAN.md
# Section 5.5), this is just for proving the mechanism works.
ARG BASE_IMAGE=pw-audio-router:dev
FROM ${BASE_IMAGE}

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        python3-pip \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --break-system-packages --no-cache-dir "aiosendspin[server]"

COPY spike03_sendspin_pushstream.py /spike03_sendspin_pushstream.py
COPY spike03_pipewire_capture_to_sendspin.py /spike03_pipewire_capture_to_sendspin.py
