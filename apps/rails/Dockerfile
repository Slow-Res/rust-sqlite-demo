# The Rails copy of the library API (apps/web), for the load-test comparison.
# Build from this directory:  docker build -t library-rails:lt .
# Run the request tests:      docker build --target test -t library-rails:test .   (see README.md)
ARG RUBY_VERSION=4.0.5

# --- base: what runs the app: curl for the container health check, jemalloc ---
FROM ruby:${RUBY_VERSION}-slim AS base
WORKDIR /app
ENV RAILS_ENV=production \
    BUNDLE_DEPLOYMENT=1 \
    BUNDLE_PATH=/usr/local/bundle \
    BUNDLE_WITHOUT="development:test" \
    RUBY_YJIT_ENABLE=1
RUN apt-get update -qq \
 && apt-get install --no-install-recommends -y curl libjemalloc2 \
 && ln -s /usr/lib/$(uname -m)-linux-gnu/libjemalloc.so.2 /usr/local/lib/libjemalloc.so \
 && rm -rf /var/lib/apt/lists /var/cache/apt/archives
ENV LD_PRELOAD=/usr/local/lib/libjemalloc.so

# --- build: compilers for the native gems (argon2), only in this stage ---
FROM base AS build
RUN apt-get update -qq \
 && apt-get install --no-install-recommends -y build-essential libpq-dev libyaml-dev git pkg-config
COPY Gemfile Gemfile.lock ./
RUN bundle install \
 && rm -rf ~/.bundle /usr/local/bundle/ruby/*/cache /usr/local/bundle/ruby/*/bundler/gems/*/.git

# --- app: the gems without compilers, the code, a non-root user ---
FROM base AS app
COPY --from=build /usr/local/bundle /usr/local/bundle
COPY app app
COPY bin bin
COPY config config
COPY lib lib
COPY config.ru Rakefile Gemfile Gemfile.lock ./
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin rails \
 && mkdir -p log tmp && chown rails:rails log tmp
USER 10001:10001

# --- test: the app plus test/, run with RAILS_ENV=test against a Postgres loaded with migrations/web-postgres ---
FROM app AS test
ENV RAILS_ENV=test
COPY --chown=rails:rails test test
CMD ["bin/rails", "test"]

# --- final: the image the load test runs (the last stage is the default target) ---
FROM app AS final
EXPOSE 4000
# 0.0.0.0:4000, WEB_CONCURRENCY workers x RAILS_MAX_THREADS threads (config/puma.rb).
CMD ["bundle", "exec", "puma", "-C", "config/puma.rb"]
