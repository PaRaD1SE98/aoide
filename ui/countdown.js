// 小窗的自动关闭倒计时（词条入库提示、提示小窗共用）：底部进度条从满格缩到空，和计时器同步；
// 鼠标停在小窗上时一起暂停，移开后继续

/** bar：进度条元素；onDone：倒计时结束时调用。返回 restart(ms)：从满格重新开始 */
function createCountdown(bar, onDone) {
  let timer;
  let total = 0;
  let remaining = 0; // 距离结束还剩多久
  let startedAt = 0;
  let paused = false;

  function run() {
    clearTimeout(timer);
    startedAt = performance.now();
    bar.style.animationPlayState = paused ? "paused" : "running";
    if (!paused) timer = setTimeout(onDone, remaining);
  }

  document.body.addEventListener("mouseenter", () => {
    if (paused) return;
    paused = true;
    clearTimeout(timer);
    remaining = Math.max(0, remaining - (performance.now() - startedAt));
    bar.style.animationPlayState = "paused";
  });
  document.body.addEventListener("mouseleave", () => {
    paused = false;
    run();
  });

  return {
    restart(ms) {
      total = remaining = ms;
      bar.style.animation = "none";
      void bar.offsetWidth; // 让动画从头播放
      bar.style.animation = "";
      bar.style.animationDuration = `${total}ms`;
      run();
    },
    stop() {
      clearTimeout(timer);
      bar.parentElement.style.visibility = "hidden";
    },
  };
}
