ARG DOCKER_TAG=latest
FROM espressif/idf:${DOCKER_TAG}

ENV LC_ALL=C.UTF-8
ENV LANG=C.UTF-8

RUN apt-get update -y && apt-get install -y \
    udev \
    curl \
    nodejs \
    clangd \
    npm

# Installer Claude Code via npm
RUN npm install -g @anthropic-ai/claude-code

# Configurer le PATH pour les shells interactifs
RUN echo 'export PATH="/root/.local/bin:$PATH"' >> /root/.bashrc && \
    echo "source /opt/esp/idf/export.sh > /dev/null 2>&1" >> /root/.bashrc

ENTRYPOINT [ "/opt/esp/entrypoint.sh" ]
CMD ["/bin/bash", "-c"]