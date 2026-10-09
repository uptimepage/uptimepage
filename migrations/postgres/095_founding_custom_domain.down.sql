UPDATE plans SET custom_domain_enabled = false, updated_at = now() WHERE id = 'founding';

UPDATE plans SET
    description = 'For a product that needs the status page on its own domain',
    updated_at  = now()
WHERE id = 'pro';
