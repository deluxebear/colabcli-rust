# The base is the project-owned image built by tools/linux-validation/Dockerfile.
FROM colabcli-rust-linux-validation:rust-1.85.1
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends dbus gnome-keyring python3-keyring \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /validation/home/runtime \
    && chown 1000:1000 /validation/home/runtime \
    && chmod 700 /validation/home/runtime
USER 1000:1000
ENV XDG_RUNTIME_DIR=/validation/home/runtime COLAB_NATIVE_STORE_TEST_ISOLATED=1
CMD ["dbus-run-session", "--config-file=/workspace/tools/secret-service-validation/session.conf", "--", "python3", "-B", "/workspace/tools/secret-service-validation/run.py"]
