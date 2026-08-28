const copyButton = document.querySelector("[data-copy]");

copyButton?.addEventListener("click", async () => {
  const label = copyButton.querySelector(".copy-label");
  try {
    await navigator.clipboard.writeText(copyButton.dataset.copy);
    label.textContent = "Copied";
    window.setTimeout(() => {
      label.textContent = "Copy";
    }, 1600);
  } catch {
    label.textContent = "Select command";
  }
});
