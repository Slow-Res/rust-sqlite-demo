# Cluster mode: WEB_CONCURRENCY worker processes of RAILS_MAX_THREADS threads each.
# The app is loaded once in the master and forked (preload), so workers share
# the code pages copy-on-write.
threads_count = Integer(ENV.fetch("RAILS_MAX_THREADS", 5))
threads threads_count, threads_count

workers Integer(ENV.fetch("WEB_CONCURRENCY", 2))
preload_app!

bind "tcp://0.0.0.0:#{ENV.fetch("PORT", 4000)}"
environment ENV.fetch("RAILS_ENV", "production")
