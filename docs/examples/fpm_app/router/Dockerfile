FROM nginx:1.27-alpine
# curl is needed by dcd's release healthcheck (exec_in: {project}-router -> curl the release).
RUN apk add --no-cache curl
COPY router.conf /etc/nginx/conf.d/default.conf
COPY maintenance.html /usr/share/nginx/html/maintenance.html
