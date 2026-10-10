#!/bin/bash
# Runs main as a Toolforge job, passing on all arguments; once, or on a
# cron schedule with --schedule. E.g.
#   ./run_job.sh restore --id 12345
#   ./run_job.sh --schedule "17 3 * * *" archive
# Uses the image built by build.sh. Output: /data/project/quickstatements/qs-<command>.{out,err}
set -euo pipefail

usage() {
	echo "Usage: $0 [--schedule \"<cron>\"] <command> [more arguments for main]" >&2
	exit 1
}

schedule=()
if [ "${1:-}" = --schedule ]; then
	[ $# -ge 3 ] || usage
	schedule=(--schedule "$2")
	shift 2
fi
[ $# -ge 1 ] || usage
[[ $1 != -* ]] || usage
command=$1
shift
job="qs-${command//_/-}"
config=/data/project/quickstatements/rust/quickstatements_rs/config_rs.json
logs=/data/project/quickstatements

# Never replace a job that may still be running
if toolforge jobs list | grep -qw -- "$job"; then
	echo "Job $job exists, running or finished. Check: toolforge jobs show $job" >&2
	echo "Remove it once finished: toolforge jobs delete $job" >&2
	exit 1
fi

args=""
if [ $# -gt 0 ]; then
	args=$(printf ' %q' "$@")
fi

toolforge jobs run ${schedule[@]+"${schedule[@]}"} --mem 2Gi --cpu 1 --mount=all \
	--image tool-quickstatements/tool-quickstatements:latest \
	--command "target/release/main --command $command --config-file $config$args" \
	--filelog -o "$logs/$job.out" -e "$logs/$job.err" \
	"$job"
echo "Started $job. Follow: tail -f $logs/$job.out $logs/$job.err"
if [ ${#schedule[@]} -gt 0 ]; then
	echo "It runs on schedule '${schedule[1]}' until removed: toolforge jobs delete $job"
fi
