FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libstdc++6 openssh-server openssl procps python3 tar wget && \
    rm -rf /var/lib/apt/lists/* && rm -f /etc/ssh/ssh_host_*
RUN useradd --create-home --shell /bin/bash visorci && \
    chmod 0700 /home/visorci && \
    printf '[user]\ndefault=visorci\n[interop]\nappendWindowsPath=false\n' > /etc/wsl.conf
COPY hypermesh-visor hypermesh-companion host-agent host-cli conductor task.py ssh_host.py hypermesh-vscode.vsix SOURCE_REVISIONS /opt/visor-wsl/
COPY vscode-server.tar.gz /tmp/vscode-server.tar.gz
RUN install -d -o visorci -g visorci -m 0700 /home/visorci/.visor-wsl \
    /home/visorci/.visor-wsl/runtime /home/visorci/.visor-wsl/project /home/visorci/.visor-wsl/config && \
    install -d -o visorci -g visorci /home/visorci/.vscode-server && \
    printf 'export HYPERMESH_RUNTIME_DIR=/home/visorci/.visor-wsl/runtime\n' > /home/visorci/.vscode-server/server-env-setup && \
    runuser -u visorci -- install -d /home/visorci/.vscode-server/data/Machine && \
    printf '{"hypermesh.companionExecutable":"/opt/visor-wsl/hypermesh-companion"}\n' > /home/visorci/.vscode-server/data/Machine/settings.json && \
    mkdir /tmp/visor-vscode-server && \
    tar -xzf /tmp/vscode-server.tar.gz --strip-components=1 -C /tmp/visor-vscode-server && \
    runuser -u visorci -- /tmp/visor-vscode-server/bin/code-server \
      --server-data-dir /home/visorci/.vscode-server \
      --extensions-dir /home/visorci/.vscode-server/extensions \
      --install-extension /opt/visor-wsl/hypermesh-vscode.vsix --force && \
    rm -rf /tmp/visor-vscode-server /tmp/vscode-server.tar.gz && \
    chmod 0755 /opt/visor-wsl/hypermesh-visor /opt/visor-wsl/hypermesh-companion
