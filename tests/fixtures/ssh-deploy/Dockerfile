# A stand-in for a real deploy target: sshd, the docker CLI, and nothing of dcd.
# The host's docker socket is mounted in at run time, so containers this image
# creates are real containers on the host daemon — which is what makes a deploy
# driven entirely over ssh observable from the test process.
FROM alpine:3.20

RUN apk add --no-cache openssh-server docker-cli docker-cli-compose \
 && ssh-keygen -A \
 && sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config \
 && mkdir -p /root/.ssh \
 && chmod 700 /root/.ssh

COPY authorized_key /root/.ssh/authorized_keys
RUN chmod 600 /root/.ssh/authorized_keys

EXPOSE 22
# sshd is NOT pid 1: the transport-loss tests kill it on purpose, and a target
# that vanishes with its sshd cannot be inspected afterwards — which is exactly
# where the state written before the link dropped has to be checked. Backgrounded
# sshd + a pid 1 that outlives it keeps the target there to look at.
CMD ["sh", "-c", "/usr/sbin/sshd -e && exec tail -f /dev/null"]
