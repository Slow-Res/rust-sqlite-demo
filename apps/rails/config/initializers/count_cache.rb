# Built once in the master, before the fork: every worker then has its own copy.
Rails.application.config.after_initialize { CountCache.shared }
