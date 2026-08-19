FROM python:3.12-slim-bookworm

ENV PYTHONUNBUFFERED=1 \
    HOME=/tmp

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        chromium \
        fonts-liberation \
        libasound2 \
        libatk-bridge2.0-0 \
        libatk1.0-0 \
        libgbm1 \
        libgtk-3-0 \
        libnss3 \
        libx11-xcb1 \
        libxcb-dri3-0 \
        libxcomposite1 \
        libxdamage1 \
        libxrandr2 \
        libxshmfence1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 app

WORKDIR /app
COPY main.py /app/main.py
RUN chown -R app:app /app

RUN pip install --no-cache-dir \
    'DrissionPage>=4.1.1.4' \
    'requests>=2.32.0'

USER app
ENTRYPOINT ["python", "/app/main.py"]
CMD ["--serve", "--headless", "--host", "0.0.0.0"]
