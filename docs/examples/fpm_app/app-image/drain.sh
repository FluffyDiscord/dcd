#!/bin/sh
# Graceful, blocking drain of the OLD release container, run by dcd's `release.drain`
# before it `rm -f`s the container. Stops nginx first (QUIT — finishes in-flight, then
# exits), then php-fpm (QUIT — completes active FastCGI requests). supervisorctl blocks
# until each program stops or its stopwaitsecs elapses.
exec supervisorctl -s unix:///run/supervisor.sock stop nginx php-fpm
