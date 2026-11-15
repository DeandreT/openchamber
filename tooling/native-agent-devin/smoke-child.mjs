/** Observe only the retained smoke child. Errors are data until the caller awaits cleanup. */
export function watchSmokeChild(child) {
  let outcome;
  const exited = new Promise((resolve) => {
    const finish = (value) => {
      child.removeListener('exit', onExit);
      child.removeListener('error', onError);
      outcome = value;
      resolve(value);
    };
    const onExit = (code, signal) => finish({ code, signal });
    const onError = (error) => finish({ error });
    child.once('exit', onExit);
    child.once('error', onError);
  });
  return {
    get outcome() { return outcome; },
    async stop(timeout = 5_000) {
      if (!outcome && !child.kill()) throw new Error(`Isolated OpenCode child ${child.pid ?? 'unspawned'} could not be stopped.`);
      let timer;
      try {
        const result = await Promise.race([exited, new Promise((_resolve, reject) => {
          timer = setTimeout(() => reject(new Error(`Isolated OpenCode child ${child.pid ?? 'unspawned'} exit was not acknowledged.`)), timeout);
        })]);
        if (result.error) throw result.error;
        return { pid: child.pid, code: result.code, signal: result.signal, acknowledged: true };
      } finally { clearTimeout(timer); }
    },
  };
}
