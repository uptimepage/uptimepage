-- Founding includes one custom domain. The flag cannot count; the operator
-- provisions each domain by hand and keeps Founding to one.
UPDATE plans SET custom_domain_enabled = true, updated_at = now() WHERE id = 'founding';

UPDATE plans SET
    description = 'For a product that needs the status page fully under its own brand',
    updated_at  = now()
WHERE id = 'pro';
