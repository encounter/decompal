document.querySelectorAll('form[data-loading]').forEach((form) => {
  const loadingText = form.getAttribute('data-loading');
  const submitButton = form.querySelector('[type="submit"]');
  if (!submitButton || !(submitButton instanceof HTMLButtonElement)) {
    return;
  }
  form.addEventListener('submit', () => {
    submitButton.disabled = true;
    submitButton.setAttribute('aria-busy', 'true');
    submitButton.innerText = loadingText || 'Loading...';
  });
});

const versionSelect = document.querySelector<HTMLSelectElement>(
  'select[name="default_version"]',
);
const categorySelect = document.querySelector<HTMLSelectElement>(
  'select[name="default_category"]',
);
if (versionSelect && categorySelect) {
  versionSelect.addEventListener('change', () => {
    const template = Array.from(
      document.querySelectorAll<HTMLTemplateElement>(
        'template[data-category-version]',
      ),
    ).find(
      (template) => template.dataset.categoryVersion === versionSelect.value,
    );
    const previous = categorySelect.value;
    categorySelect.replaceChildren(
      template?.content.cloneNode(true) ?? new Option('All', ''),
    );
    categorySelect.value = Array.from(categorySelect.options).some(
      (option) => option.value === previous,
    )
      ? previous
      : '';
  });
}
