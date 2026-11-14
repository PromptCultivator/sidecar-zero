FROM gcc:14
COPY probes/ /src/
RUN mkdir /probes && cd /src && for f in *.c; do gcc -O2 -Wall -pthread -o /probes/${f%.c} "$f" || exit 1; done
