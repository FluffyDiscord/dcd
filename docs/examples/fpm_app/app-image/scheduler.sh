#!/bin/sh
# The scheduler service's main process: run the app's due tasks once a minute.
# A foreground loop (not cron) so it inherits the container env — DATABASE_URL and the
# other secrets arrive via the container's env_file; cron would strip them from the job.
#
# Gated by SCHEDULER_ENABLED so scheduled commands run ONLY on prod: dcd sets SCHEDULER_ENABLED=1
# for the prod stage (stages.prod.compose.env); other stages (beta) leave it unset, so the container
# stays up (dcd's `pgrep -f scheduler.sh` wait still passes) but idles.
set -e
cd /app
if [ "${SCHEDULER_ENABLED:-0}" != "1" ]; then
    echo "scheduler.sh: SCHEDULER_ENABLED!=1 — idling (scheduled tasks run only on prod)"
    while true; do sleep 3600; done
fi
while true; do
    php bin/console schedule:run --no-interaction || true
    sleep 60
done
