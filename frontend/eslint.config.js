import js from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";
import eslintConfigPrettier from "eslint-config-prettier";

export default tseslint.config(
  // Global ignores
  {
    ignores: ["dist/", "node_modules/"],
  },

  // Base JS recommended rules
  js.configs.recommended,

  // TypeScript recommended rules
  ...tseslint.configs.recommended,

  // React hooks plugin
  {
    plugins: {
      "react-hooks": reactHooks,
    },
    rules: {
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",
    },
  },

  // Project-specific rule overrides
  {
    rules: {
      // Relax stylistic rules to warnings
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
      "@typescript-eslint/no-explicit-any": "warn",

      // Allow empty functions (common in React callbacks)
      "@typescript-eslint/no-empty-function": "off",

      // Allow empty catch blocks (common pattern for fire-and-forget)
      "no-empty": ["error", { allowEmptyCatch: true }],
    },
  },

  // Prettier must be last to disable conflicting rules
  eslintConfigPrettier,
);
