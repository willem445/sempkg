FROM python:3.11-slim
WORKDIR /app

RUN pip install --no-cache-dir fastapi uvicorn[standard] python-multipart

COPY src/sempkg_registry /app/sempkg_registry

ENV SEMBUNDLE_REGISTRY_ADMIN_PASSWORD=changeme
ENV STORAGE_DIR=/data/bundles
ENV CONFIG_DIR=/data/config

VOLUME ["/data"]
EXPOSE 8765

CMD ["python", "-m", "sempkg_registry", "serve", "--host", "0.0.0.0", "--port", "8765"]
