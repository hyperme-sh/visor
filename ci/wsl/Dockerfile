FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libstdc++6 procps python3 tar unzip wget && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --shell /bin/bash visorci && \
    printf '[user]\ndefault=visorci\n[interop]\nappendWindowsPath=false\n' > /etc/wsl.conf
COPY hypermesh-visor hypermesh-companion task.py hypermesh-vscode.vsix SOURCE_REVISIONS /opt/visor-wsl/
RUN install -d -o visorci -g visorci -m 0700 /tmp/visor-wsl \
    /tmp/visor-wsl/runtime /tmp/visor-wsl/project /tmp/visor-wsl/config && \
    install -d -o visorci -g visorci /home/visorci/.vscode-server && \
    printf 'export HYPERMESH_RUNTIME_DIR=/tmp/visor-wsl/runtime\n' > /home/visorci/.vscode-server/server-env-setup && \
    runuser -u visorci -- install -d /home/visorci/.vscode-server/data/Machine && \
    printf '{"hypermesh.companionExecutable":"/opt/visor-wsl/hypermesh-companion"}\n' > /home/visorci/.vscode-server/data/Machine/settings.json && \
    runuser -u visorci -- unzip -q /opt/visor-wsl/hypermesh-vscode.vsix 'extension/*' -d /home/visorci/.vscode-server && \
    runuser -u visorci -- mkdir /home/visorci/.vscode-server/extensions && \
    runuser -u visorci -- mv /home/visorci/.vscode-server/extension /home/visorci/.vscode-server/extensions/fyberlabs.hypermesh-vscode-0.1.0 && \
    chmod 0755 /opt/visor-wsl/hypermesh-visor /opt/visor-wsl/hypermesh-companion
