# One sshd, one user per login shell. sshd hands the ssh command line to the
# USER'S LOGIN SHELL, so the only way to know dcd's transport survives fish, tcsh
# or ksh is to have sshd actually run it under them.
FROM alpine:3.20

RUN apk add --no-cache openssh-server bash zsh mksh dash fish tcsh loksh yash \
 && ssh-keygen -A

# busybox sh, then every shell a server might give a deploy user.
RUN for entry in \
      "shsh:/bin/sh" \
      "shdash:/usr/bin/dash" \
      "shbash:/bin/bash" \
      "shzsh:/bin/zsh" \
      "shmksh:/bin/mksh" \
      "shloksh:/bin/ksh" \
      "shyash:/usr/bin/yash" \
      "shfish:/usr/bin/fish" \
      "shtcsh:/bin/tcsh" \
    ; do \
      user="${entry%%:*}"; shell="${entry##*:}"; \
      adduser -D -s "$shell" "$user"; \
      sed -i "s|^$user:!|$user:*|" /etc/shadow; \
      mkdir -p "/home/$user/.ssh"; \
    done

COPY authorized_key /tmp/authorized_key
RUN for user in shsh shdash shbash shzsh shmksh shloksh shyash shfish shtcsh; do \
      cp /tmp/authorized_key "/home/$user/.ssh/authorized_keys"; \
      chown -R "$user:$user" "/home/$user/.ssh"; \
      chmod 700 "/home/$user/.ssh"; \
      chmod 600 "/home/$user/.ssh/authorized_keys"; \
    done && rm /tmp/authorized_key

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
