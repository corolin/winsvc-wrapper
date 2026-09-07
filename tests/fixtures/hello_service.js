// Long-running service fixture for runtime tests.
// Prints READY, echoes argv (verifying argument escaping), ticks, and exits
// cleanly on ctrl-c (SIGINT is synthesized from console ctrl events on Windows).
let n = 0;
process.argv.slice(2).forEach((v, i) => console.log(`ARG${i}=${v}`));
console.log('READY');
process.on('SIGINT', () => {
    console.log('CLEAN-SHUTDOWN');
    process.exit(0);
});
setInterval(() => console.log(`TICK ${n++}`), 300);
