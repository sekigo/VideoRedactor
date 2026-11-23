const fileInput = document.querySelector("#video-file");
const editor = document.querySelector("#trim-editor");
const preview = document.querySelector("#preview");
const start = document.querySelector("#start");
const end = document.querySelector("#end");
const startTime = document.querySelector("#start-time");
const endTime = document.querySelector("#end-time");
const clipDuration = document.querySelector("#clip-duration");
const playClip = document.querySelector("#play-clip");
const exportButton = document.querySelector("#export");
const statusMessage = document.querySelector("#status");

let previewUrl;
let previewingClip = false;

function formatTime(seconds) {
    const centiseconds = Math.round(seconds * 100);
    const minutes = Math.floor(centiseconds / 6000);
    const remainingSeconds = ((centiseconds % 6000) / 100).toFixed(2);
    return minutes + ":" + remainingSeconds.padStart(5, "0");
}

function updateSelection() {
    const from = start.valueAsNumber;
    const to = end.valueAsNumber;
    const valid = to > from;

    startTime.value = formatTime(from);
    endTime.value = formatTime(to);
    clipDuration.value = formatTime(Math.max(0, to - from));
    start.setAttribute("aria-valuetext", startTime.value);
    end.setAttribute("aria-valuetext", endTime.value);
    playClip.disabled = !valid;
    exportButton.disabled = !valid;
    statusMessage.textContent = valid ? "" : "Конец фрагмента должен быть позже начала.";
}

fileInput.addEventListener("change", () => {
    previewingClip = false;
    preview.pause();
    editor.hidden = true;
    playClip.disabled = true;
    exportButton.disabled = true;
    preview.removeAttribute("src");

    if (previewUrl) {
        URL.revokeObjectURL(previewUrl);
        previewUrl = undefined;
    }

    const file = fileInput.files[0];
    if (!file) {
        preview.load();
        statusMessage.textContent = "";
        return;
    }

    statusMessage.textContent = "Открываю видео…";
    previewUrl = URL.createObjectURL(file);
    preview.src = previewUrl;
});

preview.addEventListener("loadedmetadata", () => {
    if (!Number.isFinite(preview.duration) || preview.duration <= 0) {
        statusMessage.textContent = "Не удалось определить длительность видео.";
        return;
    }

    start.max = end.max = String(preview.duration);
    start.value = "0";
    end.value = end.max;
    editor.hidden = false;
    updateSelection();
});

preview.addEventListener("error", () => {
    if (preview.error && preview.hasAttribute("src")) {
        editor.hidden = true;
        playClip.disabled = true;
        exportButton.disabled = true;
        statusMessage.textContent = "Браузер не смог открыть видео. Выбери другой MP4.";
    }
});

for (const slider of [start, end]) {
    slider.addEventListener("input", () => {
        previewingClip = false;
        preview.pause();
        preview.currentTime = slider.valueAsNumber;
        updateSelection();
    });
}

playClip.addEventListener("click", async () => {
    preview.currentTime = start.valueAsNumber;
    previewingClip = true;
    statusMessage.textContent = "";
    try {
        await preview.play();
    } catch {
        previewingClip = false;
        statusMessage.textContent = "Не удалось воспроизвести фрагмент.";
    }
});

preview.addEventListener("timeupdate", () => {
    if (previewingClip && preview.currentTime >= end.valueAsNumber) {
        preview.pause();
        preview.currentTime = end.valueAsNumber;
        previewingClip = false;
    }
});

preview.addEventListener("pause", () => {
    previewingClip = false;
});

const concatVideos = document.querySelector("#concat-videos");
const concatButton = document.querySelector("#concat-submit");

function updateConcatButton() {
    concatButton.disabled = concatVideos.files.length < 2;
}

concatVideos.addEventListener("change", updateConcatButton);
updateConcatButton();
